//! Isengard `dashboard` plugin (controller-side).
//!
//! Embeds a Nuxt 3 SPA bundle and serves it on a configurable HTTP port via
//! axum. v1 ships scaffold + bundle pipeline; API + WebSocket land in 5b.

#![allow(clippy::result_large_err)]

pub mod api;
pub mod approvals;
pub mod backup;
pub mod compose_diff;
pub mod containers;
pub mod deployment_groups;
pub mod deployments;
pub mod dto;
pub mod enrollment;
pub mod policies;
pub mod routing;
pub mod secrets;
pub mod webhooks;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, Uri, header};
#[allow(unused_imports)] // TODO(5b): IntoResponse used by API/WS handlers
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use isengard_controller::ControllerHandles;
use isengard_core::{Capability, CoreError, Plugin, PluginContext, PluginRegistration, Result};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{info, warn};

const PLUGIN_NAME: &str = "dashboard";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9418";

#[derive(RustEmbed)]
#[folder = "web/.output/public/"]
struct WebAssets;

#[derive(Debug, Deserialize, Default)]
struct DashboardConfig {
    #[serde(default)]
    bind_addr: Option<String>,
}

pub struct Dashboard {
    bind_addr: SocketAddr,
    server_task: Option<JoinHandle<()>>,
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.parse().unwrap(),
            server_task: None,
        }
    }
}

impl Default for Dashboard {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn build_router(handles: Option<Arc<ControllerHandles>>) -> Router {
    let mut router = Router::new()
        .route("/_nuxt/{*path}", get(serve_asset_nuxt))
        .route("/", get(serve_index));

    if let Some(h) = handles {
        let ws_router = Router::new()
            .route("/ws/events", get(ws::ws_handler))
            .route(
                "/api/v1/services/{stack_id}/{service_name}/logs/ws",
                get(ws::handle_service_logs),
            )
            .with_state(h.clone());

        let install_router = Router::new()
            .route("/install.sh", get(api::install_sh))
            .with_state(h.clone());

        router = router
            .nest("/api/v1", api::router(h.clone()))
            .nest("/api/v1", routing::router(h.clone()))
            .nest("/api/v1", deployments::router(h.clone()))
            .nest("/api/v1", deployment_groups::router(h.clone()))
            .nest("/api/v1", policies::router(h.clone()))
            .nest("/api/v1", approvals::router(h.clone()))
            .nest("/api/v1", webhooks::router(h.clone()))
            .nest("/api/v1", backup::router(h.clone()))
            .nest("/api/v1", secrets::router(h.clone()))
            .nest("/api/v1", enrollment::router(h))
            .merge(ws_router)
            .merge(install_router);
    }

    router.fallback(get(fallback_handler))
}

async fn serve_index() -> Response {
    serve_embedded("index.html").await
}

async fn serve_asset_nuxt(Path(path): Path<String>) -> Response {
    // Nuxt assets live under _nuxt/ in the bundle. Reconstruct path.
    let asset_path = format!("_nuxt/{path}");
    serve_embedded(&asset_path).await
}

async fn fallback_handler(uri: Uri) -> Response {
    // Try literal path first (favicon, robots.txt, etc), then SPA fallback.
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && WebAssets::get(path).is_some() {
        return serve_embedded(path).await;
    }
    // SPA fallback: any unknown route serves index.html so client-side routing handles it.
    serve_embedded("index.html").await
}

async fn serve_embedded(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache_control_for(path))
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("not found: {path}")))
            .unwrap(),
    }
}

fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("_nuxt/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

#[async_trait]
impl Plugin for Dashboard {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: DashboardConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing dashboard config: {e}")))?;

        let addr_str = cfg.bind_addr.as_deref().unwrap_or(DEFAULT_BIND_ADDR);
        self.bind_addr = addr_str
            .parse()
            .map_err(|e| init_err(format!("invalid bind_addr {addr_str}: {e}")))?;

        info!(addr = %self.bind_addr, "dashboard initialised");
        Ok(())
    }

    async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
        let handles = ctx
            .bus
            .clone()
            .and_then(|b| b.downcast::<isengard_controller::ControllerHandles>().ok());

        if handles.is_none() {
            warn!("dashboard started without ControllerHandles, API routes will not be mounted");
        }

        let app = build_router(handles);
        let addr = self.bind_addr;

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| start_err(format!("bind {addr}: {e}")))?;

        let task = tokio::spawn(async move {
            info!(%addr, "dashboard server listening");
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "dashboard server ended");
            }
        });

        self.server_task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(t) = self.server_task.take() {
            t.abort();
        }
        info!("dashboard stopped");
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(Dashboard::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_new_has_default_bind_addr() {
        let d = Dashboard::new();
        assert_eq!(d.bind_addr.to_string(), "0.0.0.0:9418");
    }

    #[test]
    fn router_builds_without_panic() {
        let _ = build_router(None);
    }
}
