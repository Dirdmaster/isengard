//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

use anyhow::Result;
use isengard_core::{HostMode, Plugin, PluginContext, registrations_for};
use tracing::{info, instrument};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller, e.g. `http://controller.example.com:9417`.
    /// Required — Phase 2d removes the `Option` wrapper from Phase 1.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
}

pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

#[instrument(skip(opts))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(controller = %opts.controller_url, state_dir = ?opts.state_dir, "starting agent");
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
        let ctx = PluginContext::new(HostMode::Agent, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    for mut plugin in started {
        plugin.stop().await?;
    }

    info!("agent exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn dummy() {
        // placeholder; real tests come in Tasks 2-4 of Phase 2d
        assert_eq!(2 + 2, 4);
    }
}
