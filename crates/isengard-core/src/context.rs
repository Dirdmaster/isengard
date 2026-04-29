//! `PluginContext`: services the host exposes to plugins via their lifecycle hooks.
//!
//! Phase 1 minimum: host mode + plugin's slice of the merged config. Subsequent
//! phases will add: logger handle, journal writer, gRPC clients, secret store.

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub mode: HostMode,
    /// The plugin's slice of the merged configuration tree. Empty `Value::Null`
    /// when the plugin has no configuration.
    pub config: serde_json::Value,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: serde_json::Value) -> Self {
        Self { mode, config }
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
    }

    #[test]
    fn plugin_context_carries_arbitrary_config() {
        let cfg = serde_json::json!({"interval": "30m", "watch_all": true});
        let ctx = PluginContext::new(HostMode::Controller, cfg.clone());
        assert_eq!(ctx.config["interval"], "30m");
    }
}
