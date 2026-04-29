//! Controller-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC server, no inventory store.

use anyhow::Result;
use isengard_core::{registrations_for, HostMode, Plugin, PluginContext};
use tracing::{info, instrument};

/// Options for running the controller.
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// Optional config tree (per-plugin slices keyed by plugin name).
    pub config: serde_json::Value,
}

impl Default for ControllerOptions {
    fn default() -> Self {
        Self { config: serde_json::Value::Object(Default::default()) }
    }
}

/// Discover and instantiate every plugin that advertises `Capability::Controller`.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Controller)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Run controller-mode lifecycle: init → start every plugin, then wait. Stop
/// every plugin on `ctx_token` cancellation. Phase 1 returns immediately
/// (no event loop yet) — subsequent phases hold the runner open on tokio
/// signal.
#[instrument(skip(opts))]
pub async fn run_controller(opts: ControllerOptions) -> Result<()> {
    info!("starting controller");
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

    let mut started = Vec::with_capacity(plugins.len());

    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let ctx = PluginContext::new(HostMode::Controller, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // Phase 1: stop everything immediately and return. Phase 2+ replaces this
    // with a tokio::signal::ctrl_c().await + per-plugin task supervision.
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!("controller exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_controller_loads_zero_or_more_plugins_and_returns_ok() {
        // No plugins are registered against Capability::Controller in the
        // controller crate's own test cfg — that's fine, this asserts the
        // runner doesn't blow up on an empty plugin set or any plugin set.
        let res = run_controller(ControllerOptions::default()).await;
        assert!(res.is_ok(), "run_controller failed: {:?}", res);
    }
}
