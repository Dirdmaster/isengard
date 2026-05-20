//! `PluginContext`: services the host exposes to plugins via their lifecycle
//! hooks.
//!
//! Minimum: host mode + the plugin's slice of the merged config. Subsequent
//! phases will add: logger handle, journal writer, gRPC clients, secret
//! store.

use std::any::Any;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ApprovalStore, EventEmitter, HostId, PolicyLoader, UpdateDispatcher};

/// Which mode the host is running in.
///
/// Affects which capability sub-traits a plugin's lifecycle hooks are called
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    /// Host is running as the controller (central control plane).
    Controller,
    /// Host is running as an agent on a fleet node.
    Agent,
}

/// Context handed to a plugin during `init`/`start`.
///
/// Cheap to clone: Arc-backed fields will land in later phases. Construct
/// with [`PluginContext::new`] and chain `with_*` builders to attach the
/// optional services the host wires up.
#[derive(Clone)]
pub struct PluginContext {
    /// Which mode the host is running in.
    pub mode: HostMode,
    /// The plugin's slice of the merged configuration tree.
    ///
    /// Empty `Value::Null` when the plugin has no configuration.
    pub config: serde_json::Value,
    /// Optional event sink.
    ///
    /// `None` on the controller in phase 4a; `Some` on the agent once 4b
    /// wires the gRPC client through.
    pub events: Option<Arc<dyn EventEmitter>>,
    /// Optional opaque handle passed by the controller host to plugins.
    ///
    /// Currently carries `Arc<ControllerHandles>` (inventory + journal + bus
    /// bundle); downcast in plugin `start()` to access concrete state. Field
    /// name is retained for backward compatibility (see the TODO in
    /// isengard-controller for a future migration of `ControllerHandles`
    /// to isengard-core).
    pub bus: Option<Arc<dyn Any + Send + Sync>>,
    /// Optional sink the updater plugin consults before recreating a
    /// container.
    ///
    /// Wired by the agent host so the `DeploymentSupervisor` can intercept
    /// and run a blue-green driver instead of an in-place recreate. `None`
    /// outside the agent.
    pub update_dispatcher: Option<Arc<dyn UpdateDispatcher>>,
    /// Stable identifier for the host running this plugin.
    ///
    /// Set by the agent runtime once enrollment + local DB lookup have
    /// produced a [`HostId`]; `None` outside the agent (controller mode,
    /// plugin loaders in unit tests).
    pub host_id: Option<HostId>,
    /// Optional policy loader.
    ///
    /// Plugins that respect update policies (the updater, primarily) call
    /// `list()` to fetch the current policy snapshot before deciding whether
    /// to act on a candidate. `None` outside the agent or when the agent
    /// runtime hasn't wired the loader yet (older agents, certain test
    /// harnesses).
    pub policy_loader: Option<Arc<dyn PolicyLoader>>,
    /// Optional approval store.
    ///
    /// The updater plugin persists pending approvals via this seam when a
    /// candidate's resolved policy gates on `Approval`. `None` outside the
    /// agent or in test harnesses that don't exercise the approval path.
    pub approval_store: Option<Arc<dyn ApprovalStore>>,
}

impl PluginContext {
    /// Build a context with the bare minimum: mode + config.
    ///
    /// Chain `with_*` calls to attach optional services.
    pub fn new(mode: HostMode, config: serde_json::Value) -> Self {
        Self {
            mode,
            config,
            events: None,
            bus: None,
            update_dispatcher: None,
            host_id: None,
            policy_loader: None,
            approval_store: None,
        }
    }

    /// Attach an [`EventEmitter`] so plugins can emit journal events.
    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    /// Attach an opaque host-provided handle (controller mode).
    pub fn with_bus(mut self, bus: Arc<dyn Any + Send + Sync>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Attach an [`UpdateDispatcher`] so the updater can hand triggers to
    /// the blue-green supervisor (agent mode).
    pub fn with_update_dispatcher(mut self, d: Arc<dyn UpdateDispatcher>) -> Self {
        self.update_dispatcher = Some(d);
        self
    }

    /// Attach the [`HostId`] of the running agent.
    pub fn with_host_id(mut self, host_id: HostId) -> Self {
        self.host_id = Some(host_id);
        self
    }

    /// Attach a [`PolicyLoader`] so policy-respecting plugins (the updater,
    /// primarily) can fetch the current snapshot.
    pub fn with_policy_loader(mut self, loader: Arc<dyn PolicyLoader>) -> Self {
        self.policy_loader = Some(loader);
        self
    }

    /// Attach an [`ApprovalStore`] so the updater can persist
    /// pending-approval rows when the resolved gate is `Approval`.
    pub fn with_approval_store(mut self, store: Arc<dyn ApprovalStore>) -> Self {
        self.approval_store = Some(store);
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
            .field(
                "update_dispatcher",
                &self.update_dispatcher.as_ref().map(|_| "<dispatcher>"),
            )
            .field(
                "policy_loader",
                &self.policy_loader.as_ref().map(|_| "<policy_loader>"),
            )
            .field(
                "approval_store",
                &self.approval_store.as_ref().map(|_| "<approval_store>"),
            )
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
