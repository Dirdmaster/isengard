//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

use anyhow::Result;
use isengard_core::{registrations_for, HostMode, Plugin, PluginContext};
use tracing::{info, instrument};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller (`https://host:port`). Unused in Phase 1; gRPC
    /// client lands in Phase 2.
    pub controller_url: Option<String>,
    pub config: serde_json::Value,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            controller_url: None,
            config: serde_json::Value::Object(Default::default()),
        }
    }
}

pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

#[instrument(skip(opts))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(controller = ?opts.controller_url, "starting agent");
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
    use super::*;

    #[tokio::test]
    async fn run_agent_returns_ok_with_default_options() {
        let res = run_agent(AgentOptions::default()).await;
        assert!(res.is_ok(), "run_agent failed: {:?}", res);
    }
}
