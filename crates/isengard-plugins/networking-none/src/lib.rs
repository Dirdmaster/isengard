//! No-op `NetworkingAdapter` baseline. `expose()` returns an error explaining
//! that no adapter is configured. Always available so an agent without
//! explicit configuration has a well-defined adapter.

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;
use isengard_core::registration::{Capability, PluginRegistration};

#[derive(Default)]
pub struct NoneAdapter;

#[async_trait]
impl Plugin for NoneAdapter {
    fn name(&self) -> &'static str {
        "networking-none"
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
impl NetworkingAdapter for NoneAdapter {
    fn id(&self) -> &'static str {
        "none"
    }
    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }
    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }
    async fn expose(&self, _ctx: &AdapterContext, _spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        Err(CoreError::Other(
            "no adapter configured: install + enable a NetworkingAdapter (tailscale, cf-tunnel) to expose hostnames"
                .into(),
        ))
    }
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        Ok(())
    }
    fn tls_strategy(&self) -> TlsStrategy {
        TlsStrategy::PassThrough
    }
}

inventory::submit! {
    PluginRegistration {
        name: "networking-none",
        capabilities: &[Capability::Agent],
        constructor: || Box::new(NoneAdapter) as Box<dyn Plugin>,
    }
}
