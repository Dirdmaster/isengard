//! In-memory fanout from agent-emitted `LogChunk` frames to the
//! per-WebSocket-client receiver each dashboard subscription owns.
//!
//! The dashboard plugin calls `register(sub_id)` to get an `mpsc::Receiver`
//! for a fresh subscription. The controller's Sync handler routes inbound
//! `AgentMessage::LogChunk` frames into the matching sender via `route()`.
//! When the WebSocket client disconnects, the dashboard plugin calls
//! `unregister(sub_id)` and the next agent-emitted chunk for the dead
//! subscription is dropped at the controller boundary.

use std::collections::HashMap;
use std::sync::Arc;

use isengard_proto::pb::LogChunk;
use tokio::sync::{Mutex, mpsc};
use tracing::debug;

/// Per-subscription bounded queue. Sized for ~256 lines of head-room before
/// `try_send` returns `Full`; on overflow the controller emits a `dropped`
/// frame to the WebSocket client and continues.
const QUEUE_SIZE: usize = 256;

/// Registry: subscription_id -> Sender. Receivers are owned by whoever called
/// `register` (the WebSocket task).
#[derive(Default)]
pub struct LogFanout {
    senders: Mutex<HashMap<String, mpsc::Sender<LogChunk>>>,
}

impl LogFanout {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a fresh subscription. Returns the receiver the WebSocket task
    /// reads from. Idempotent: re-registering the same id replaces the
    /// previous sender (the previous receiver simply stops getting frames).
    pub async fn register(&self, sub_id: String) -> mpsc::Receiver<LogChunk> {
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        self.senders.lock().await.insert(sub_id, tx);
        rx
    }

    /// Drop the registration. After this returns, any further `route()` for
    /// the id is a no-op.
    pub async fn unregister(&self, sub_id: &str) {
        self.senders.lock().await.remove(sub_id);
    }

    /// Forward an agent-emitted chunk to the matching subscription. Returns
    /// `Outcome::Routed` on success, `Outcome::Dropped` on a full queue, and
    /// `Outcome::Unknown` when the subscription isn't registered (typical
    /// after a client disconnect; the controller-side `StopLogStream` raced
    /// the agent's last in-flight chunks).
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

/// Outcome of a single `route()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Routed,
    Dropped,
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
