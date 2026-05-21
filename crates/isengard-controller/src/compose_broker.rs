//! Compose-write request and ack matcher (v0.3d).
//!
//! Flow for the dashboard's `PUT /api/v1/stacks/<id>/compose`:
//!
//! 1. Mint a fresh `request_id` (ULID).
//! 2. Register a oneshot via [`ComposeBroker::register`].
//! 3. Send `WriteCompose { request_id, ... }` down the host's Sync
//!    stream.
//! 4. Await the matching `WriteComposeAck`, which the controller's
//!    Sync handler resolves via [`ComposeBroker::resolve`] when the ack
//!    lands on the inbound stream.
//!
//! Timeouts are the caller's responsibility: the dashboard handler
//! wraps the await in `tokio::time::timeout` and surfaces 504 when the
//! agent is wedged. The broker has [`ComposeBroker::cancel`] for the
//! timeout path so the registration doesn't leak.
//!
//! Per-stack queueing is intentionally absent. Two concurrent dashboard
//! PUTs against the same stack are not serialised here; the agent-side
//! `compose_writer` hash check makes the second one fail with
//! `KIND_CONFLICT` rather than corrupt the file, which is the right
//! semantics for "two operators editing the same stack at the same
//! time".

use std::collections::HashMap;
use std::sync::Arc;

use isengard_proto::pb::WriteComposeAck;
use tokio::sync::{Mutex, oneshot};

/// Request-id keyed registry of pending compose-write oneshots.
#[derive(Default)]
pub struct ComposeBroker {
    /// `request_id` -> sender for the awaiting dashboard handler.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<WriteComposeAck>>>>,
}

impl ComposeBroker {
    /// Builds an empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a oneshot for `request_id`.
    ///
    /// The dashboard handler awaits the returned receiver. Two
    /// registrations for the same id are a programmer error (request
    /// ids are agent-unique ULIDs); the second supersedes the first
    /// and the first hangs until its own timeout fires.
    pub async fn register(&self, request_id: String) -> oneshot::Receiver<WriteComposeAck> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().await;
        guard.insert(request_id, tx);
        rx
    }

    /// Resolves a pending request with the agent's ack.
    ///
    /// Returns `false` when no registration exists for `request_id`
    /// (e.g. the dashboard timed out and dropped its receiver, or the
    /// agent replied for a request we don't remember). Both are normal
    /// at boundary conditions and the caller logs at `debug`.
    pub async fn resolve(&self, ack: WriteComposeAck) -> bool {
        let mut guard = self.pending.lock().await;
        let Some(tx) = guard.remove(&ack.request_id) else {
            return false;
        };
        tx.send(ack).is_ok()
    }

    /// Drops a registration without resolving it.
    ///
    /// The dashboard handler's timeout path calls this so the sender
    /// doesn't leak forever.
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
