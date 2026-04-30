//! Controller-mode runtime: load plugins, bind the gRPC server, await
//! shutdown, drain, stop plugins, exit.

mod auth;
mod service;

pub mod bus;
pub mod plugin_host;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use isengard_core::{HostMode, Plugin, registrations_for};
use isengard_proto::FILE_DESCRIPTOR_SET;
use isengard_proto::pb::controller_server::ControllerServer;
use tokio::signal;
use tonic::transport::Server;
use tracing::{info, instrument};

pub use auth::TokenAuthLayer;
pub use service::ControllerService;

/// Options for running the controller.
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// Address the gRPC server binds to (e.g. `0.0.0.0:9417`).
    pub listen: SocketAddr,
    /// Directory where mutable state lives (the SQLite db, future config cache, etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    /// Optional config tree (per-plugin slices keyed by plugin name).
    pub config: serde_json::Value,
}

/// Discover and instantiate every plugin that advertises `Capability::Controller`.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Controller)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Run controller-mode: init+start every plugin, bind the gRPC server, await
/// ctrl_c, drain in-flight requests, stop every plugin, return.
#[instrument(skip(opts), fields(listen = %opts.listen))]
pub async fn run_controller(opts: ControllerOptions) -> Result<()> {
    info!("starting controller");

    // -- inventory + journal + event bus -------------------------------------
    let db_path = opts.state_dir.join("isengard.db");
    let inventory = Arc::new(
        isengard_storage::Inventory::open(&db_path)
            .await
            .with_context(|| format!("opening inventory at {db_path:?}"))?,
    );
    info!(?db_path, "inventory opened");

    // Journal shares the same SQLite file. Inventory::open already ran the
    // migrations (including 0002_events.sql); Journal::open re-runs migrate!
    // which is idempotent.
    let journal = Arc::new(
        isengard_storage::Journal::open(&db_path)
            .await
            .with_context(|| format!("opening journal at {db_path:?}"))?,
    );

    let bus = Arc::new(crate::bus::EventBus::new());

    // -- plugin init/start ----------------------------------------------------
    // Load + start controller-side plugins (notifier, etc).
    let mut controller_plugins =
        plugin_host::load_controller_plugins(bus.clone(), journal.clone(), opts.config.clone())
            .await;
    info!(
        plugin_count = controller_plugins.len(),
        "controller plugins started"
    );

    // -- gRPC server ----------------------------------------------------------
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()
        .context("building reflection service")?;

    // Build the auth layer from the env var. Binary's main() already verified
    // it's set; if it's somehow missing here, fail loud rather than silently allow.
    let token =
        std::env::var("ISENGARD_TOKEN").with_context(|| "ISENGARD_TOKEN env var must be set")?;
    let auth_layer = crate::auth::TokenAuthLayer::new(token);

    let svc = ControllerServer::new(ControllerService::new(inventory, journal, bus));

    info!("gRPC server listening");
    let serve_fut = Server::builder()
        .layer(auth_layer)
        .add_service(svc)
        .add_service(reflection)
        .serve_with_shutdown(opts.listen, async {
            // SIGINT / Ctrl-C terminates serve_with_shutdown gracefully.
            // serve waits for in-flight requests to drain before returning.
            let _ = signal::ctrl_c().await;
            info!("shutdown signal received, draining");
        });

    serve_fut.await.context("gRPC server error")?;

    // -- plugin stop ----------------------------------------------------------
    plugin_host::stop_controller_plugins(&mut controller_plugins).await;

    info!("controller exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration tests against a running server live in
    // `tests/server_skeleton.rs`. The Phase-1-style "default options" smoke
    // test no longer applies because the runner now blocks on ctrl_c.
    #[test]
    fn dummy() {
        // placeholder so the unit-test target stays alive for nextest
        assert_eq!(2 + 2, 4);
    }
}
