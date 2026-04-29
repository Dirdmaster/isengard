//! Event types emitted by plugins and consumed by the journal + subscribers.
//!
//! Phase 1 contains only the minimal shape used by the plugin trait. The
//! journal/subscriber wiring lands in Phase 4.

use serde::{Deserialize, Serialize};

/// Stable identifier for the kind of event. Used by `EventSubscriber` to filter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UpdateChecked,
    UpdateSuccess,
    UpdateFailed,
    UpdateSkipped,
    AgentConnect,
    AgentDisconnect,
    PluginCrashed,
}

/// A journal event. The payload is plugin-defined JSON; concrete schemas are
/// owned by each plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub ts_millis: u64,
    pub host_id: Option<String>,
    pub container_id: Option<String>,
    pub plugin: Option<String>,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_round_trips_through_json() {
        let kind = EventKind::UpdateSuccess;
        let s = serde_json::to_string(&kind).unwrap();
        assert_eq!(s, "\"update_success\"");
        let back: EventKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, EventKind::UpdateSuccess);
    }

    #[test]
    fn event_serialises_with_optionals() {
        let evt = Event {
            kind: EventKind::AgentConnect,
            ts_millis: 1_700_000_000_000,
            host_id: Some("01J...".into()),
            container_id: None,
            plugin: Some("agent".into()),
            payload: serde_json::json!({"version": "0.1.0-alpha"}),
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("\"kind\":\"agent_connect\""));
        assert!(s.contains("\"host_id\":\"01J...\""));
        assert!(s.contains("\"container_id\":null"));
    }
}
