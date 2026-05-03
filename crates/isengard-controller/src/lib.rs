//! Controller-mode runtime: load plugins, bind the gRPC server, await
//! shutdown, drain, stop plugins, exit.

mod auth;
mod service;

pub mod bus;
pub mod disconnect_monitor;
pub mod pending_actions;
pub mod plugin_host;
pub mod routing;
pub mod sync_services;
pub mod sync_stacks;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use isengard_core::{Event, HostMode, Plugin, registrations_for};
use isengard_proto::FILE_DESCRIPTOR_SET;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_storage::{InsertEvent, Inventory, Journal};
use tokio::signal;
use tonic::transport::Server;
use tracing::{info, instrument};

pub use auth::TokenAuthLayer;
pub use service::ControllerService;

use crate::bus::EventBus;

/// Bundle of references controller-side plugins need to access controller state.
/// Passed via `PluginContext.bus` (downcast from `Arc<dyn Any>`).
#[derive(Clone)]
pub struct ControllerHandles {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
    pub routing: Arc<routing::RoutingPusher>,
}

/// Journal an event then broadcast it on the bus. Used by both the Sync
/// handler (for agent-originated events) and controller-internal producers
/// like `disconnect_monitor`.
///
/// On journal write failure, broadcasts NO event — better to drop than to
/// notify on something we have no record of.
pub async fn persist_and_broadcast(journal: &Journal, bus: &EventBus, event: Event) {
    let insert = InsertEvent {
        host_id: event.host_id.map(|id| id.into()),
        kind: event.kind.clone(),
        container_name: event.container_name.clone(),
        image: event.image.clone(),
        old_digest: event.old_digest.clone(),
        new_digest: event.new_digest.clone(),
        error: event.error.clone(),
        summary: event.summary.clone(),
        metadata_json: if event.metadata.is_null() {
            None
        } else {
            Some(event.metadata.to_string())
        },
        occurred_at: event.occurred_at,
    };
    if let Err(e) = journal.insert(insert).await {
        tracing::warn!(error = %e, kind = %event.kind, "journal.insert failed; dropping event");
        return;
    }
    bus.publish(event);
}

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

    let routing = Arc::new(routing::RoutingPusher::new(inventory.clone()));

    // -- plugin init/start ----------------------------------------------------
    // Load + start controller-side plugins (notifier, etc).
    let handles = Arc::new(ControllerHandles {
        inventory: inventory.clone(),
        journal: journal.clone(),
        bus: bus.clone(),
        routing: routing.clone(),
    });
    let mut controller_plugins =
        plugin_host::load_controller_plugins(handles, opts.config.clone()).await;
    info!(
        plugin_count = controller_plugins.len(),
        "controller plugins started"
    );

    // Background task: detect long-disconnected agents and emit
    // `agent.disconnect_long` (4h threshold, 60s poll cadence in production).
    let disconnect_monitor = std::sync::Arc::new(disconnect_monitor::DisconnectMonitor::new(
        inventory.clone(),
        journal.clone(),
        bus.clone(),
        14400, // 4 hours
        60.0,  // 60s poll
    ));
    let disconnect_handle = disconnect_monitor.start();

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

    let svc = ControllerServer::new(ControllerService::new(
        inventory,
        journal,
        bus,
        routing.clone(),
    ));

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

    // -- shutdown -------------------------------------------------------------
    disconnect_handle.abort();

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
