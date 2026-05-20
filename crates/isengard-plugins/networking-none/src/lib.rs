//! Baseline `NetworkingAdapter` that exposes nothing.
//!
//! The agent always has exactly one networking adapter active. When the
//! operator hasn't configured one explicitly, this no-op fills the slot
//! so the rest of the agent doesn't have to special-case `Option<Adapter>`.
//!
//! [`NoneAdapter::expose`] returns an error pointing the operator at the
//! real adapters (`tailscale`, `cf-tunnel`). Join and leave are infallible
//! no-ops; TLS strategy is [`TlsStrategy::PassThrough`] so the controller
//! does no ACME work for hosts on this adapter.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;
use isengard_core::registration::{Capability, PluginRegistration};

/// The no-op adapter. Stateless; constructed via `Default`.
///
/// Registered into the inventory under the name `networking-none` and the
/// adapter id `none`. The agent picks this when no `[networking]` block is
/// configured.
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
    /// Always fails. The operator must enable a real adapter to expose
    /// hostnames.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] with a message pointing at the
    /// available adapters.
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
