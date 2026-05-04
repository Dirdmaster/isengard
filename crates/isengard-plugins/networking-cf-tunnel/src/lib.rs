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
use serde::Deserialize;

pub mod api;
pub mod cloudflared;

#[derive(Deserialize)]
struct CfTunnelConfig {
    api_token: String,
    account_id: String,
    zone_id: String,
    tunnel_id: Option<String>,
    tunnel_name: Option<String>,
    tunnel_token: Option<String>,
}

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

    async fn join(&self, ctx: &AdapterContext) -> Result<()> {
        cloudflared::ensure_present()?;

        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;

        let api = api::CfApi::new(cfg.api_token.clone());

        let tunnel_token = if let Some(t) = cfg.tunnel_token.clone() {
            t
        } else {
            // First-time setup: create the tunnel via API and surface the new
            // tunnel_id + tunnel_token back to the caller via journal/log.
            // (Persisting them back to adapter_config is a Plan C / settings-UI
            // concern; for v1 the operator copy-pastes them into settings.)
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

        let _ = ctx; // silence if unused
        Ok(())
    }

    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }

    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
        let api = api::CfApi::new(cfg.api_token.clone());

        let tunnel_id = cfg.tunnel_id.clone().ok_or_else(|| {
            CoreError::Other(
                "expose called before tunnel_id was provisioned (run join first or set in settings)"
                    .into(),
            )
        })?;

        // Replace the entire ingress with our single hostname rule + a 404
        // catch-all. Multi-rule per-tunnel support is a v1.x follow-up.
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

    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()> {
        let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
        let api = api::CfApi::new(cfg.api_token.clone());

        let hostname = endpoint_id.trim_start_matches("cf-tunnel:");

        // DNS cleanup: list + delete records pointing at this hostname.
        // We log on failure rather than swallow — a leaked DNS record points
        // at an offline tunnel target and produces user-visible errors.
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

        // Reset ingress to the catch-all 404. Same warn-on-failure: a stale
        // ingress rule points cloudflared at a port that may now be reused.
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
