//! In-memory fanout from agent `LogChunk` frames to per-WebSocket
//! receivers.
//!
//! The dashboard plugin calls [`LogFanout::register`] for a fresh
//! subscription and reads chunks from the returned `mpsc::Receiver`.
//! The controller's Sync handler routes inbound `AgentMessage::LogChunk`
//! frames through [`LogFanout::route`]. When the WebSocket client
//! disconnects, the dashboard plugin calls [`LogFanout::unregister`]
//! and subsequent chunks for the dead subscription drop at the
//! controller boundary.

use std::collections::HashMap;
use std::sync::Arc;

use isengard_proto::pb::LogChunk;
use tokio::sync::{Mutex, mpsc};
use tracing::debug;

/// Per-subscription bounded queue size.
///
/// 256 lines of head-room before `try_send` returns `Full`; on
/// overflow the controller emits a `dropped` frame to the WebSocket
/// client and continues.
const QUEUE_SIZE: usize = 256;

/// Registry mapping `subscription_id` to its outbound sender.
///
/// Receivers are owned by whoever called [`LogFanout::register`] (the
/// WebSocket task).
#[derive(Default)]
pub struct LogFanout {
    /// `subscription_id` -> outbound sender.
    senders: Mutex<HashMap<String, mpsc::Sender<LogChunk>>>,
}

impl LogFanout {
    /// Builds a shareable empty fanout.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers a fresh subscription.
    ///
    /// Returns the receiver the WebSocket task reads from. Idempotent:
    /// re-registering the same id replaces the previous sender, and
    /// the previous receiver stops getting frames.
    pub async fn register(&self, sub_id: String) -> mpsc::Receiver<LogChunk> {
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        self.senders.lock().await.insert(sub_id, tx);
        rx
    }

    /// Drops the registration. Subsequent [`LogFanout::route`] calls
    /// for the id no-op.
    pub async fn unregister(&self, sub_id: &str) {
        self.senders.lock().await.remove(sub_id);
    }

    /// Forwards an agent-emitted chunk to the matching subscription.
    ///
    /// Returns [`Outcome::Routed`] on success, [`Outcome::Dropped`] on a
    /// full queue, and [`Outcome::Unknown`] when the subscription isn't
    /// registered. The `Unknown` case is typical after a client
    /// disconnect: the controller-side `StopLogStream` raced the
    /// agent's last in-flight chunks.
    pub async fn route(&self, chunk: LogChunk) -> Outcome {
        let sub_id = chunk.subscription_id.clone();
        let map = self.senders.lock().await;
        let Some(tx) = map.get(&sub_id) else {
            debug!(sub = %sub_id, "log_fanout: chunk for unknown subscription, dropping");
            return Outcome::Unknown;
        };
        match tx.try_send(chunk) {
            Ok(()) => Outcome::Routed,
            Err(mpsc::error::TrySendError::Full(_)) => Outcome::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => Outcome::Unknown,
        }
    }

    /// Returns true if `sub_id` has an active sender.
    #[cfg(test)]
    pub async fn contains(&self, sub_id: &str) -> bool {
        self.senders.lock().await.contains_key(sub_id)
    }
}

/// Outcome of a single [`LogFanout::route`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Chunk enqueued for the subscriber.
    Routed,
    /// Subscriber's queue was full; chunk dropped at the controller
    /// boundary.
    Dropped,
    /// No subscription with that id.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_proto::pb::log_chunk;

    fn line(sub: &str, msg: &str) -> LogChunk {
        LogChunk {
            subscription_id: sub.into(),
            kind: log_chunk::Kind::Line as i32,
            occurred_at: "2026-05-06T00:00:00Z".into(),
            stream: "stdout".into(),
            line: msg.into(),
            dropped: 0,
            reason: String::new(),
        }
    }

    #[tokio::test]
    async fn register_then_route_delivers() {
        let f = LogFanout::new();
        let mut rx = f.register("sub-a".into()).await;
        let outcome = f.route(line("sub-a", "hello")).await;
        assert_eq!(outcome, Outcome::Routed);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.line, "hello");
    }

    #[tokio::test]
    async fn unregister_drops_routes() {
        let f = LogFanout::new();
        let _rx = f.register("sub-a".into()).await;
        f.unregister("sub-a").await;
        let outcome = f.route(line("sub-a", "hello")).await;
        assert_eq!(outcome, Outcome::Unknown);
    }

    #[tokio::test]
    async fn unknown_subscription_returns_unknown() {
        let f = LogFanout::new();
        let outcome = f.route(line("sub-x", "hello")).await;
        assert_eq!(outcome, Outcome::Unknown);
    }

    #[tokio::test]
    async fn full_queue_returns_dropped() {
        let f = LogFanout::new();
        let _rx = f.register("sub-a".into()).await;
        // Saturate the queue without draining it.
        for i in 0..QUEUE_SIZE {
            let outcome = f.route(line("sub-a", &format!("msg-{i}"))).await;
            assert_eq!(outcome, Outcome::Routed, "unexpected drop at {i}");
        }
        // QUEUE_SIZE+1 should be dropped.
        let outcome = f.route(line("sub-a", "overflow")).await;
        assert_eq!(outcome, Outcome::Dropped);
    }
}
