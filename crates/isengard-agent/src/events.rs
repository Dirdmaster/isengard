//! Agent-side EventEmitter that queues events onto an mpsc channel for the
//! sync loop to drain and multiplex onto the Sync stream.
//!
//! Backpressure policy: if the channel is full (sync stream is slow, or
//! disconnected), `emit` drops the event and logs a WARN. v1 doesn't persist
//! outbound events.

use async_trait::async_trait;
use isengard_core::{Event, EventEmitter};
use tokio::sync::mpsc;
use tracing::warn;

/// Bounded queue depth for outbound events. Backpressure drops the
/// newest event on overflow.
const OUTBOUND_CAPACITY: usize = 256;

/// Agent-side [`EventEmitter`] implementation. Queues events onto an
/// internal mpsc that the sync loop drains and multiplexes onto the
/// outbound Sync stream as `AgentMessage::Event` frames.
pub struct OutboundEmitter {
    /// Sender half of the bounded queue. Receiver lives on the sync loop.
    tx: mpsc::Sender<Event>,
}

impl OutboundEmitter {
    /// Returns the emitter and the matching Receiver for the sync loop.
    pub fn new() -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        (Self { tx }, rx)
    }

    /// Hand out a clone of the inner sender so non-plugin subsystems (e.g.
    /// the proxy healthcheck loop) can publish onto the same outbound queue
    /// the sync loop drains. They can't satisfy `EventEmitter`'s `Arc<dyn>`
    /// shape because they live behind `&self` borrows, so they hold a raw
    /// `Sender<Event>` instead.
    pub fn sender(&self) -> mpsc::Sender<Event> {
        self.tx.clone()
    }
}

#[async_trait]
impl EventEmitter for OutboundEmitter {
    async fn emit(&self, event: Event) {
        if let Err(e) = self.tx.try_send(event) {
            match e {
                mpsc::error::TrySendError::Full(ev) => {
                    warn!(
                        kind = %ev.kind,
                        "outbound event dropped: channel full"
                    );
                }
                mpsc::error::TrySendError::Closed(ev) => {
                    warn!(
                        kind = %ev.kind,
                        "outbound event dropped: channel closed (sync loop ended)"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn emit_delivers_to_receiver() {
        let (emitter, mut rx) = OutboundEmitter::new();
        emitter.emit(ev("update.success")).await;
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind, "update.success");
    }

    #[tokio::test]
    async fn emit_does_not_block_when_channel_full() {
        let (emitter, _rx) = OutboundEmitter::new();
        // Fill the channel to capacity (don't drain).
        for i in 0..OUTBOUND_CAPACITY {
            emitter.emit(ev(&format!("k{i}"))).await;
        }
        // One more: should drop, not block.
        let extra = ev("overflow");
        let started = std::time::Instant::now();
        emitter.emit(extra).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }
}
