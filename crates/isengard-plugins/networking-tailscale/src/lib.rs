//! Tailscale `NetworkingAdapter` for Isengard.
//!
//! Drives the operator's installed `tailscale` CLI via tokio subprocesses.
//! No Go FFI: the user installs tailscale once, this adapter shells out.
//!
//! # Flow
//!
//! `join`: checks `tailscale` is on PATH and the backend is `Running`.
//! Refuses to come up if the operator hasn't run `tailscale up` yet.
//!
//! `expose`: runs `tailscale serve --bg --https=443` pointed at the local
//! listener port. Optionally flips `tailscale funnel` on if the spec
//! requests public reachability. Fetches the leaf cert + key via
//! `tailscale cert` and returns them inside `adapter_data` so the agent's
//! `CertStore` can install them.
//!
//! `unexpose`: tears down the serve and funnel state. See the note on
//! [`TailscaleAdapter::unexpose`] for the global-port limitation.
//!
//! TLS strategy is [`TlsStrategy::AdapterProvided`]: tailscale issues the
//! leaf cert and the controller does no ACME work for these hostnames.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;
use serde_json::json;

pub mod cli;

/// Tailscale adapter. Stateless; constructed via `Default`.
///
/// Registered as `networking-tailscale` (plugin name) and `tailscale`
/// (adapter id). The agent selects it via `[networking] adapter =
/// "tailscale"`.
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

    /// Verifies the `tailscale` CLI is installed and the backend is up.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when `tailscale` is missing from `PATH`
    /// or when `tailscale status --json` reports the backend isn't
    /// `Running` (operator hasn't run `tailscale up` yet).
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

    /// Wires `public_hostname` to `local_listener_port` over HTTPS.
    ///
    /// Runs `tailscale serve --bg --https=443` then optionally
    /// `tailscale funnel on` when `adapter_specific.funnel` is true. Pulls
    /// the issued cert via `tailscale cert <hostname>` and surfaces it
    /// in `adapter_data` for the agent to install into its `CertStore`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when any of the subprocess invocations
    /// fail or when the cert files can't be read from the temp dir.
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

    /// Tears down the serve and funnel state on port 443.
    ///
    /// **v1 limitation:** `tailscale serve --https=443 off` is global per
    /// port: it tears down every serve config on 443, not only the one
    /// for `endpoint_id`. Multi-hostname-per-host setups need
    /// per-path serve config (`--set-path=/<unique>`) before this
    /// becomes safe. Deferred until a multi-hostname tailscale user
    /// surfaces.
    ///
    /// Funnel teardown is best-effort: idempotent off-calls swallow
    /// failures because `funnel off` is allowed to run when funnel was
    /// never on.
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        if let Err(e) = cli::serve_off().await {
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
