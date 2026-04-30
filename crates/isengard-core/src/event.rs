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
