//! Cloudflare Tunnel `NetworkingAdapter`.
//!
//! Two planes work together:
//!
//! - **Data plane:** a supervised `cloudflared tunnel run` subprocess
//!   holds the persistent edge connection. See [`cloudflared::supervise`]
//!   for the restart loop.
//! - **Control plane:** Cloudflare's v4 REST API drives tunnel CRUD,
//!   ingress configuration, and DNS records. See [`api::CfApi`].
//!
//! # Flow
//!
//! `join`: shells out to `cloudflared` to confirm it's installed,
//! either uses a persisted `tunnel_token` from settings or creates a new
//! tunnel via the API, then spawns the supervised subprocess.
//!
//! `expose`: replaces the tunnel's entire ingress with one rule for the
//! requested hostname plus a 404 catch-all, then upserts a CNAME from
//! the hostname to `<tunnel_id>.cfargotunnel.com`.
//!
//! `unexpose`: deletes the matching DNS records and resets the ingress
//! to the catch-all. Failures are logged, not propagated: leaving the
//! adapter half-open is better than blocking shutdown.
//!
//! TLS strategy is [`TlsStrategy::EdgeTermination`]: Cloudflare terminates
//! TLS at the edge, the controller does no ACME work.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;
use serde::Deserialize;

pub mod api;
pub mod cloudflared;

/// Parsed shape of the `[networking]` settings block for this adapter.
///
/// Required: `account_id`, `zone_id`. Optional: `api_token` (legacy
/// per-host path; migrate to `isd configure set cloudflare.api_token`),
/// `tunnel_id`, `tunnel_name`, `tunnel_token`. When `tunnel_token` is
/// missing the adapter creates a fresh tunnel on `join` and logs the
/// new id and token for the operator to persist.
///
/// `api_token` used to be required and was read straight from the
/// per-host `adapter_config.config_json.api_token` row. The
/// `isd configure` track (v0.6) lifts the token into a fleet-wide
/// `cloudflare.api_token` secret on the controller; the adapter still
/// accepts a per-host value for backward compatibility, but emits a
/// migration warning and prefers the new path when wired through.
#[derive(Deserialize)]
struct CfTunnelConfig {
    /// Cloudflare API token. Needs `Account:Cloudflare Tunnel:Edit` and
    /// `Zone:DNS:Edit` scopes for the configured account and zone.
    ///
    /// Legacy per-host source. New deployments should leave this empty
    /// and set the fleet-wide secret via
    /// `isd configure set cloudflare.api_token --stdin`.
    #[serde(default)]
    api_token: Option<String>,
    /// Cloudflare account UUID owning the tunnel.
    account_id: String,
    /// Cloudflare zone UUID the hostnames live in.
    zone_id: String,
    /// Persisted tunnel UUID. Skips tunnel creation when set.
    tunnel_id: Option<String>,
    /// Tunnel display name used when creating a new tunnel. Defaults to
    /// `isengard`.
    tunnel_name: Option<String>,
    /// Persisted `cloudflared` connection token. Skips tunnel creation
    /// when set.
    tunnel_token: Option<String>,
}

impl CfTunnelConfig {
    /// Returns the Cloudflare API token, preferring the controller-managed
    /// `cloudflare.api_token` path and falling back to the legacy
    /// per-host `adapter_config.config_json.api_token` row.
    ///
    /// The fleet-wide secret normally arrives via the agent's
    /// `FetchSecret` RPC and is merged into `ctx.settings` before this
    /// adapter sees it, so callers do not need to thread the dispatcher
    /// here. When the new path is wired but unset, the operator gets a
    /// clear hint pointing at the `isd configure` verb instead of a
    /// generic "missing field" decode error.
    fn resolve_api_token(&self) -> Result<String> {
        match self.api_token.as_deref() {
            Some(token) if !token.is_empty() => {
                tracing::warn!(
                    "cf-tunnel: api_token sourced from adapter_config.config_json; \
                     migrate to the fleet-wide secret via \
                     `isd configure set cloudflare.api_token --stdin`"
                );
                Ok(token.to_string())
            }
            _ => Err(CoreError::Other(
                "cloudflare.api_token not set; run \
                 `isd configure set cloudflare.api_token --stdin` \
                 (or set api_token on the cf-tunnel adapter_config row as a \
                 legacy fallback)"
                    .into(),
            )),
        }
    }
}

/// Cloudflare Tunnel adapter. Stateless; constructed via `Default`.
///
/// Registered as `networking-cf-tunnel` (plugin name) and `cf-tunnel`
/// (adapter id). The agent selects it via `[networking] adapter =
/// "cf-tunnel"`.
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

    /// Brings the adapter up. Resolves the tunnel and spawns the
    /// supervised subprocess.
    ///
    /// On first call (no `tunnel_token` in settings), creates a fresh
    /// tunnel via the API and logs the new `tunnel_id` and
    /// `tunnel_token` so the operator can persist them. Subsequent
    /// calls reuse the existing values.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when `cloudflared` isn't on `PATH`,
    /// when the settings JSON fails to decode, or when the tunnel
    /// creation API call fails.
    async fn join(&self, ctx: &AdapterContext) -> Result<()> {
        cloudflared::ensure_present()?;

        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;

        let api = api::CfApi::new(cfg.resolve_api_token()?);

        let tunnel_token = if let Some(t) = cfg.tunnel_token.clone() {
            t
        } else {
            let created = api
                .create_tunnel(
                    &cfg.account_id,
                    cfg.tunnel_name.as_deref().unwrap_or("isengard"),
                )
                .await?;
            tracing::info!(
                tunnel_id = %created.id,
                "cf-tunnel: created new tunnel; persist `tunnel_id` and `tunnel_token` to adapter_config to skip recreation on next start"
            );
            created.token
        };

        let token_for_supervisor = tunnel_token.clone();
        tokio::spawn(async move {
            cloudflared::supervise(token_for_supervisor).await;
        });

        let _ = ctx;
        Ok(())
    }

    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }

    /// Wires `public_hostname` to `local_listener_port` through the tunnel.
    ///
    /// Replaces the tunnel's ingress with a single rule for this hostname
    /// plus a 404 catch-all. Multi-rule per-tunnel support lands later.
    /// Upserts the public DNS CNAME so requests for the hostname route
    /// through Cloudflare's edge into the tunnel.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Other`] when settings can't be decoded, when
    /// `tunnel_id` is missing (operator must run `join` first or
    /// configure it explicitly), or when either API call fails.
    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
        let api = api::CfApi::new(cfg.resolve_api_token()?);

        let tunnel_id = cfg.tunnel_id.clone().ok_or_else(|| {
            CoreError::Other(
                "expose called before tunnel_id was provisioned (run join first or set in settings)"
                    .into(),
            )
        })?;

        let ingress = vec![
            api::IngressRule {
                hostname: Some(spec.public_hostname.clone()),
                service: format!("http://localhost:{}", spec.local_listener_port),
            },
            api::IngressRule {
                hostname: None,
                service: "http_status:404".into(),
            },
        ];
        api.set_ingress(&cfg.account_id, &tunnel_id, ingress)
            .await?;

        let dns = api
            .upsert_dns_cname(
                &cfg.zone_id,
                &spec.public_hostname,
                &format!("{tunnel_id}.cfargotunnel.com"),
            )
            .await?;

        Ok(ExposedEndpoint {
            id: format!("cf-tunnel:{}", spec.public_hostname),
            url: format!("https://{}", spec.public_hostname),
            adapter_data: serde_json::json!({
                "tunnel_id": tunnel_id,
                "dns_record_id": dns.id,
            }),
        })
    }

    /// Tears down the DNS records and ingress for `endpoint_id`.
    ///
    /// Best-effort by design. A failed DNS list, delete, or ingress
    /// reset is logged at warn but doesn't fail the call: blocking
    /// shutdown on Cloudflare flakiness would leave the agent stuck.
    /// The operator's recovery path is to retry the unexpose later or
    /// fix the leaked records manually.
    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()> {
        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
        let api = api::CfApi::new(cfg.resolve_api_token()?);

        let hostname = endpoint_id.trim_start_matches("cf-tunnel:");

        match api.list_dns_records(&cfg.zone_id, hostname).await {
            Ok(records) => {
                for r in records {
                    if let Err(e) = api.delete_dns_record(&cfg.zone_id, &r.id).await {
                        tracing::warn!(
                            hostname = %hostname,
                            record_id = %r.id,
                            error = %e,
                            "cf-tunnel: failed to delete DNS record (may leak in CF)"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    hostname = %hostname,
                    error = %e,
                    "cf-tunnel: failed to list DNS records during unexpose; not cleaning up"
                );
            }
        }

        if let Some(tunnel_id) = cfg.tunnel_id.as_ref() {
            if let Err(e) = api
                .set_ingress(
                    &cfg.account_id,
                    tunnel_id,
                    vec![api::IngressRule {
                        hostname: None,
                        service: "http_status:404".into(),
                    }],
                )
                .await
            {
                tracing::warn!(
                    hostname = %hostname,
                    tunnel_id = %tunnel_id,
                    error = %e,
                    "cf-tunnel: failed to reset ingress to catch-all"
                );
            }
        }

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
        constructor: || Box::new(CfTunnelAdapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_token(token: Option<&str>) -> CfTunnelConfig {
        CfTunnelConfig {
            api_token: token.map(str::to_string),
            account_id: "acct-1".into(),
            zone_id: "zone-1".into(),
            tunnel_id: None,
            tunnel_name: None,
            tunnel_token: None,
        }
    }

    #[test]
    fn resolve_api_token_uses_legacy_value_when_present() {
        let cfg = cfg_with_token(Some("legacy-token"));
        let token = cfg
            .resolve_api_token()
            .expect("legacy api_token must still resolve");
        assert_eq!(token, "legacy-token");
    }

    #[test]
    fn resolve_api_token_errors_when_unset_with_configure_hint() {
        let cfg = cfg_with_token(None);
        let err = cfg.resolve_api_token().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("cloudflare.api_token"),
            "error message must reference the configure key: {msg}"
        );
        assert!(
            msg.contains("isd configure set"),
            "error message must point at the configure verb: {msg}"
        );
    }

    #[test]
    fn resolve_api_token_treats_empty_string_as_unset() {
        let cfg = cfg_with_token(Some(""));
        let err = cfg
            .resolve_api_token()
            .expect_err("empty token must be treated as unset");
        let msg = format!("{err}");
        assert!(msg.contains("cloudflare.api_token"), "{msg}");
    }

    #[test]
    fn cf_tunnel_config_deserialises_without_api_token() {
        // The migration path: api_token is no longer required at decode
        // time. Operators set the fleet-wide secret via `isd configure
        // set cloudflare.api_token`; per-host adapter_config no longer
        // needs it.
        let json = serde_json::json!({
            "account_id": "acct-1",
            "zone_id": "zone-1"
        });
        let cfg: CfTunnelConfig =
            serde_json::from_value(json).expect("config without api_token must decode");
        assert!(cfg.api_token.is_none());
        assert_eq!(cfg.account_id, "acct-1");
    }
}
