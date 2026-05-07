//! v0.3d compose write broker. The dashboard's `PUT /api/v1/stacks/<id>/compose`
//! handler:
//!  1. Mints a fresh `request_id` (Ulid).
//!  2. Registers a oneshot via [`ComposeBroker::register`].
//!  3. Sends `WriteCompose { request_id, ... }` down the host's Sync stream.
//!  4. Awaits the matching [`isengard_proto::pb::WriteComposeAck`] (forwarded
//!     here by the controller's gRPC service when it arrives on the inbound
//!     stream).
//!
//! Timeouts are the caller's responsibility: the dashboard handler wraps
//! the await in `tokio::time::timeout` and surfaces 504 to the operator
//! when the agent is wedged.
//!
//! Per-stack queueing: two concurrent dashboard PUTs against the same
//! stack are NOT serialized here. Conflict detection on the agent side
//! (compose_writer's hash check) makes the second one fail with
//! `KIND_CONFLICT` rather than corrupt the file, which is the right
//! semantics for "two operators editing the same stack at the same time."

use std::collections::HashMap;
use std::sync::Arc;

use isengard_proto::pb::WriteComposeAck;
use tokio::sync::{Mutex, oneshot};

#[derive(Default)]
pub struct ComposeBroker {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<WriteComposeAck>>>>,
}

impl ComposeBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a oneshot for `request_id`. The dashboard handler
    /// awaits the returned receiver. Two registrations for the same id
    /// are programmer error (request ids are agent-unique Ulids); the
    /// second supersedes the first and the first will hang until its
    /// own timeout.
    pub async fn register(&self, request_id: String) -> oneshot::Receiver<WriteComposeAck> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().await;
        guard.insert(request_id, tx);
        rx
    }

    /// Resolve a pending request. Returns `false` if no registration
    /// exists for `request_id` (e.g. the dashboard timed out and dropped
    /// its receiver, or the agent replied for a request we don't
    /// remember: both are normal at boundary conditions).
    pub async fn resolve(&self, ack: WriteComposeAck) -> bool {
        let mut guard = self.pending.lock().await;
        let Some(tx) = guard.remove(&ack.request_id) else {
            return false;
        };
        tx.send(ack).is_ok()
    }

    /// Drop a registration without resolving it. Used by the dashboard
    /// handler's timeout path so we don't leak a sender forever.
    pub async fn cancel(&self, request_id: &str) {
        let mut guard = self.pending.lock().await;
        guard.remove(request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_proto::pb::write_compose_ack::Kind;

    fn ok_ack(request_id: &str) -> WriteComposeAck {
        WriteComposeAck {
            request_id: request_id.to_string(),
            kind: Kind::Ok as i32,
            written_sha256: "abcd".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn register_and_resolve_round_trip() {
        let broker = ComposeBroker::new();
        let rx = broker.register("req-1".into()).await;
        let resolved = broker.resolve(ok_ack("req-1")).await;
        assert!(resolved);
        let ack = rx.await.unwrap();
        assert_eq!(ack.request_id, "req-1");
        assert_eq!(ack.kind, Kind::Ok as i32);
    }

    #[tokio::test]
    async fn resolve_for_unknown_id_returns_false() {
        let broker = ComposeBroker::new();
        let resolved = broker.resolve(ok_ack("never-registered")).await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let broker = ComposeBroker::new();
        let _rx = broker.register("req-2".into()).await;
        broker.cancel("req-2").await;
        let resolved = broker.resolve(ok_ack("req-2")).await;
        assert!(!resolved);
    }
}
