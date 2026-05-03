//! Cloudflare Tunnel NetworkingAdapter.
//!
//! Data plane: a supervised `cloudflared tunnel run` subprocess holds the
//! persistent connection to CF edge.
//! Control plane: CF v4 REST API for tunnel CRUD, ingress rules, DNS.

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;

pub mod api;
pub mod cloudflared;

#[derive(Default)]
pub struct CfTunnelAdapter;

#[async_trait]
impl Plugin for CfTunnelAdapter {
    fn name(&self) -> &'static str {
        "networking-cf-tunnel"
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
impl NetworkingAdapter for CfTunnelAdapter {
    fn id(&self) -> &'static str {
        "cf-tunnel"
    }

    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        Err(CoreError::Other(
            "cf-tunnel join not yet implemented (Plan B 8g-2)".into(),
        ))
    }
    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }
    async fn expose(&self, _ctx: &AdapterContext, _spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        Err(CoreError::Other(
            "cf-tunnel expose not yet implemented (Plan B 8g-2)".into(),
        ))
    }
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        Ok(())
    }
    fn tls_strategy(&self) -> TlsStrategy {
        TlsStrategy::EdgeTermination
    }
}

inventory::submit! {
    isengard_core::registration::PluginRegistration {
        name: "networking-cf-tunnel",
        capabilities: &[isengard_core::registration::Capability::Agent],
        constructor: || Box::new(CfTunnelAdapter::default()),
    }
}
