//! Tailscale NetworkingAdapter for Isengard.
//!
//! Drives the user's installed `tailscale` CLI via tokio subprocess. No Go
//! FFI; users install tailscale separately (which they almost certainly
//! already have if they're using this adapter).

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;

pub mod cli;

#[derive(Default)]
pub struct TailscaleAdapter;

#[async_trait]
impl Plugin for TailscaleAdapter {
    fn name(&self) -> &'static str {
        "networking-tailscale"
    }
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl NetworkingAdapter for TailscaleAdapter {
    fn id(&self) -> &'static str {
        "tailscale"
    }

    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        cli::ensure_present()?;
        let status = cli::status().await?;
        if !status.online {
            return Err(CoreError::Other(format!(
                "tailscale present but not online (run `tailscale up` first); state={}",
                status.backend_state
            )));
        }
        Ok(())
    }

    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }

    async fn expose(&self, _ctx: &AdapterContext, _spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        Err(CoreError::Other(
            "tailscale expose not yet implemented (Plan B 8f-2)".into(),
        ))
    }

    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        Ok(())
    }

    fn tls_strategy(&self) -> TlsStrategy {
        TlsStrategy::AdapterProvided
    }
}

inventory::submit! {
    isengard_core::registration::PluginRegistration {
        name: "networking-tailscale",
        capabilities: &[isengard_core::registration::Capability::Agent],
        constructor: || Box::new(TailscaleAdapter::default()),
    }
}
