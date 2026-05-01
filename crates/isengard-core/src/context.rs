//! `PluginContext`: services the host exposes to plugins via their lifecycle hooks.
//!
//! Phase 1 minimum: host mode + plugin's slice of the merged config. Subsequent
//! phases will add: logger handle, journal writer, gRPC clients, secret store.

use std::any::Any;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::EventEmitter;

/// Which mode the host is running in. Affects which capability sub-traits a
/// plugin's lifecycle hooks are called through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    Controller,
    Agent,
}

/// Context handed to a plugin during `init`/`start`. Cheap to clone (Arc-backed
/// fields will land in later phases).
#[derive(Clone)]
pub struct PluginContext {
    pub mode: HostMode,
    /// The plugin's slice of the merged configuration tree. Empty `Value::Null`
    /// when the plugin has no configuration.
    pub config: serde_json::Value,
    /// Optional event sink. `None` on the controller in phase 4a; `Some` on
    /// the agent once 4b wires the gRPC client through.
    pub events: Option<Arc<dyn EventEmitter>>,
    /// Optional opaque handle passed by the controller host to plugins.
    /// Currently carries `Arc<ControllerHandles>` (inventory + journal + bus bundle);
    /// downcast in plugin `start()` to access concrete state. Field name is
    /// retained for backward compatibility — see TODO in isengard-controller for
    /// a future migration of ControllerHandles to isengard-core.
    pub bus: Option<Arc<dyn Any + Send + Sync>>,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: serde_json::Value) -> Self {
        Self {
            mode,
            config,
            events: None,
            bus: None,
        }
    }

    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_bus(mut self, bus: Arc<dyn Any + Send + Sync>) -> Self {
        self.bus = Some(bus);
        self
    }
}

impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("mode", &self.mode)
            .field("config", &self.config)
            .field("events", &self.events.as_ref().map(|_| "<emitter>"))
            .field("bus", &self.bus.as_ref().map(|_| "<bus>"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mode_serialises_lowercase() {
        let s = serde_json::to_string(&HostMode::Controller).unwrap();
        assert_eq!(s, "\"controller\"");
        let s = serde_json::to_string(&HostMode::Agent).unwrap();
        assert_eq!(s, "\"agent\"");
    }

    #[test]
    fn plugin_context_constructs_with_null_config() {
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);
        assert_eq!(ctx.mode, HostMode::Agent);
        assert!(ctx.config.is_null());
        assert!(ctx.events.is_none());
    }

    #[test]
    fn plugin_context_carries_arbitrary_config() {
        let cfg = serde_json::json!({"interval": "30m", "watch_all": true});
        let ctx = PluginContext::new(HostMode::Controller, cfg.clone());
        assert_eq!(ctx.config["interval"], "30m");
    }

    #[test]
    fn plugin_context_with_events_attaches_emitter() {
        use crate::event::NoopEmitter;
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null)
            .with_events(Arc::new(NoopEmitter));
        assert!(ctx.events.is_some());
    }

    #[test]
    fn plugin_context_with_bus_attaches_handle() {
        let bus: Arc<dyn std::any::Any + Send + Sync> = Arc::new(42u32);
        let ctx = PluginContext::new(HostMode::Controller, serde_json::Value::Null).with_bus(bus);
        assert!(ctx.bus.is_some());
    }

    #[test]
    fn plugin_context_debug_elides_emitter() {
        use crate::event::NoopEmitter;
        let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null)
            .with_events(Arc::new(NoopEmitter));
        let s = format!("{ctx:?}");
        assert!(s.contains("<emitter>"));
    }
}
