//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

pub mod agent_state;
pub mod enroll;
pub mod sync;

/// Convenience alias used throughout the agent.
pub type Result<T> = anyhow::Result<T>;

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

#[instrument(skip(opts), fields(controller = %opts.controller_url))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(state_dir = ?opts.state_dir, "starting agent");

    // -- determine agent_id ----------------------------------------------
    let existing = agent_state::load(&opts.state_dir).await?;
    let agent_id = match existing {
        Some(state) => {
            info!(agent_id = %state.agent_id, "already enrolled, skipping enroll");
            state.agent_id
        }
        None => {
            info!("no agent.json found, enrolling with controller");
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let host_info = enroll::HostInfo::detect();
            let id = enroll::enroll(&opts.controller_url, &token, host_info).await?;
            agent_state::save(
                &opts.state_dir,
                &agent_state::AgentState {
                    agent_id: id.clone(),
                },
            )
            .await?;
            info!(agent_id = %id, "enrolled");
            id
        }
    };

    // -- plugin lifecycle (unchanged from Phase 1) -----------------------
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

    // Phase 2d: stop every plugin and return. Phase 2e holds the runner open
    // on a Sync stream + ctrl_c await.
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!(agent_id = %agent_id, "agent exited cleanly");
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
