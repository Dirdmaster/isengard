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
use serde_json::json;

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

    async fn expose(&self, _ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        cli::serve_https(spec.local_listener_port).await?;

        let funnel_on = spec
            .adapter_specific
            .get("funnel")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if funnel_on {
            cli::funnel_on().await?;
        }

        // Cert flow: fetch via `tailscale cert` so the first request to the
        // hostname doesn't 503. Caller (controller's RoutingPusher or agent
        // equivalent) reads the cert from adapter_data and writes it to the
        // agent's CertStore via the existing install API. The cert pump
        // hookup is a separate Phase 8c+ follow-up; for now the adapter just
        // surfaces the cert.
        let (cert_pem, key_pem) = cli::fetch_cert(&spec.public_hostname).await?;

        Ok(ExposedEndpoint {
            id: format!("tailscale:{}", spec.public_hostname),
            url: format!("https://{}", spec.public_hostname),
            adapter_data: json!({
                "funnel": funnel_on,
                "cert_pem": cert_pem,
                "key_pem": key_pem,
            }),
        })
    }

    /// **v1 limitation:** `tailscale serve --https=443 off` is GLOBAL per
    /// port — it tears down ALL serve config on 443, not just the one for
    /// `endpoint_id`. For multi-hostname-per-host setups this is wrong.
    /// Per-hostname unexpose would need `--set-path=/<unique>` or different
    /// ports; deferred until we actually have a multi-hostname tailscale user.
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        if let Err(e) = cli::serve_off().await {
            // Worth logging — if serve_off fails, the tailscale config still
            // routes 443 to a port that may now be reused. Funnel teardown
            // is best-effort (idempotent).
            tracing::warn!(error = %e, "tailscale: serve_off failed during unexpose");
        }
        let _ = cli::funnel_off().await;
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
        constructor: || Box::new(TailscaleAdapter),
    }
}
