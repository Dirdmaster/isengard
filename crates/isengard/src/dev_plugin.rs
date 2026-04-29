//! No-op `dev` plugin compiled in under the `dev` feature flag. Validates the
//! plugin host wiring while the real plugins (updater, dashboard, notifier)
//! are not yet implemented.

use anyhow::anyhow;
use async_trait::async_trait;
use isengard_core::{
    AgentPlugin, Capability, ControllerPlugin, Plugin, PluginContext, PluginRegistration, Result,
};

pub struct DevPlugin;

#[async_trait]
impl Plugin for DevPlugin {
    fn name(&self) -> &'static str { "dev" }
    fn version(&self) -> &'static str { "0.1.0-alpha" }

    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        tracing::info!(plugin = "dev", "init");
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        tracing::info!(plugin = "dev", "start");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!(plugin = "dev", "stop");
        Ok(())
    }
}

#[async_trait]
impl AgentPlugin for DevPlugin {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
}

impl ControllerPlugin for DevPlugin {}

inventory::submit! {
    PluginRegistration {
        name: "dev",
        capabilities: &[Capability::Agent, Capability::Controller],
        constructor: || Box::new(DevPlugin) as Box<dyn Plugin>,
    }
}

// `anyhow` import is kept available for future expansion of dev plugin behavior;
// silence unused-import warning under #[allow] without dropping the import.
#[allow(dead_code)]
fn _keep_anyhow_imported() -> anyhow::Result<()> {
    Err(anyhow!("never called"))
}
