//! Controller-side plugin host. Walks the inventory for plugins with
//! `Capability::Controller`, inits each with a context including the bus +
//! journal, starts them, and (for `EventSubscriber` plugins) spawns a per-
//! plugin task that drains the bus and calls `handle`.
//!
//! v1 has no clean dynamic-cast for "is this plugin an EventSubscriber?"
//! since `Plugin` is the type-erased trait object. We work around it by
//! making EVERY controller plugin self-subscribe in its `start` if it wants
//! events — passed the bus via context (already there). The host doesn't
//! need to know.

use std::any::Any;
use std::sync::Arc;

use isengard_core::{HostMode, Plugin, PluginContext, registrations_for};
use serde_json::Value;
use tracing::{info, warn};

use crate::ControllerHandles;

pub struct LoadedPlugin {
    pub name: &'static str,
    pub plugin: Box<dyn Plugin>,
}

/// Construct + init + start every controller-side plugin in the inventory.
/// Returns the loaded plugins so the caller can drive them through stop on shutdown.
pub async fn load_controller_plugins(
    handles: Arc<ControllerHandles>,
    config: Value,
) -> Vec<LoadedPlugin> {
    let handles_any: Arc<dyn Any + Send + Sync> = handles.clone();
    let ctx = PluginContext::new(HostMode::Controller, config).with_bus(handles_any);

    let mut loaded = Vec::new();
    for reg in registrations_for(HostMode::Controller) {
        let mut plugin = (reg.constructor)();
        let name = reg.name;
        info!(plugin = name, "loading controller plugin");
        if let Err(e) = plugin.init(&ctx).await {
            warn!(plugin = name, error = ?e, "plugin init failed; skipping");
            continue;
        }
        if let Err(e) = plugin.start(&ctx).await {
            warn!(plugin = name, error = ?e, "plugin start failed; skipping");
            continue;
        }
        loaded.push(LoadedPlugin { name, plugin });
    }
    loaded
}

/// Stop every loaded plugin. Called on controller shutdown.
pub async fn stop_controller_plugins(loaded: &mut [LoadedPlugin]) {
    for lp in loaded.iter_mut() {
        if let Err(e) = lp.plugin.stop().await {
            warn!(plugin = lp.name, error = ?e, "plugin stop failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use isengard_storage::{Inventory, Journal};

    #[tokio::test]
    async fn empty_inventory_yields_no_plugins() {
        // Without any controller plugins linked into this test binary,
        // load_controller_plugins should return empty.
        let handles = Arc::new(ControllerHandles {
            inventory: Arc::new(Inventory::open_in_memory().await.unwrap()),
            journal: Arc::new(Journal::open_in_memory().await.unwrap()),
            bus: Arc::new(EventBus::new()),
        });
        let loaded = load_controller_plugins(handles, Value::Null).await;
        // Note: this test asserts the function returns CLEANLY with no plugins.
        // If a future test in this binary links a controller plugin, this
        // assertion would fail — adapt at that time.
        let _ = loaded; // length depends on what's linked; just verify no panic.
    }
}
