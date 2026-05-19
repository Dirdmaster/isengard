//! Event types emitted by plugins and consumed by the journal + subscribers.
//!
//! An [`Event`] is the canonical shape carried over the wire (proto), persisted
//! by the controller's journal, and broadcast on the in-process EventBus.
//! [`EventEmitter`] is the async sink plugins write to via their
//! [`crate::PluginContext`].

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HostId;

/// Canonical event-kind strings emitted by the updater plugin. Re-exported
/// so subscribers (notifier, journal filters, dashboard streams) and tests
/// can refer to them by symbol rather than by raw string literal.
pub mod kinds {
    /// Cycle summary, one per `do_cycle` invocation.
    pub const UPDATE_CHECKED: &str = "update.checked";
    /// A `needs_update` candidate was successfully recreated.
    pub const UPDATE_SUCCESS: &str = "update.success";
    /// A `needs_update` candidate failed to recreate.
    pub const UPDATE_FAILED: &str = "update.failed";
    /// A candidate was skipped because the resolved policy said so.
    /// Emitted for `strategy=Pinned` and active `paused_until`.
    pub const UPDATE_POLICY_SKIPPED: &str = "update.policy_skipped";
    /// A `needs_update` candidate resolved with `gate=Approval`. The updater
    /// has persisted a `pending_open` approval row and is waiting on a
    /// dashboard or notifier callback decision before recreating.
    pub const UPDATE_PENDING_APPROVAL: &str = "update.pending_approval";
    /// An operator (or auto-decider) approved a pending update via the
    /// dashboard or a notifier callback. The agent will pick up the queued
    /// `apply_update` HostAction on its next sync.
    pub const UPDATE_APPROVED: &str = "update.approved";
    /// An operator rejected a pending update. No recreate happens until the
    /// digest changes again.
    pub const UPDATE_REJECTED: &str = "update.rejected";
    /// An operator snoozed a pending update. The service-scope policy is
    /// updated with `paused_until = now + N hours`; the next scan skips.
    pub const UPDATE_SNOOZED: &str = "update.snoozed";
    /// A pending approval row aged past its `expires_at` and was
    /// auto-transitioned to `pending_expired` by the controller's periodic
    /// expiry tick.
    pub const UPDATE_EXPIRED: &str = "update.expired";
    /// A `needs_update` candidate fell outside its resolved policy's
    /// maintenance window. Payload includes the `next_window`
    /// (RFC3339, UTC) so notifier consumers can quote the back-online time.
    pub const UPDATE_DEFERRED: &str = "update.deferred";
}

/// A journal event. Plugin-defined `kind` strings (e.g. "update.success",
/// "agent.connect") drive subscriber filtering. Optional fields are populated
/// when relevant; `metadata` is a free-form JSON escape hatch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Event {
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub host_id: Option<HostId>,
    pub summary: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Async sink for events emitted by plugins.
#[async_trait::async_trait]
pub trait EventEmitter: Send + Sync + 'static {
    async fn emit(&self, event: Event);
}

/// A no-op emitter for contexts where events go nowhere (e.g. unit tests).
pub struct NoopEmitter;

#[async_trait::async_trait]
impl EventEmitter for NoopEmitter {
    async fn emit(&self, _event: Event) {}
}

/// Convenience for plugins: wrap an emitter into an `Arc<dyn EventEmitter>`.
pub fn arc_emitter<E: EventEmitter>(e: E) -> Arc<dyn EventEmitter> {
    Arc::new(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialises_round_trip() {
        let e = Event {
            kind: "update.success".into(),
            occurred_at: Utc::now(),
            summary: "ok".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "update.success");
        assert_eq!(back.summary, "ok");
    }

    #[tokio::test]
    async fn noop_emitter_swallows_event() {
        let e = NoopEmitter;
        e.emit(Event::default()).await;
    }
}
