//! Controller-mode runtime: load plugins, bind the gRPC server, await
//! shutdown, drain, stop plugins, exit.

mod service;

pub mod auth;
pub mod bus;
pub mod ca;
pub mod disconnect_monitor;
pub mod enrollment;
pub mod pending_actions;
pub mod plugin_host;
pub mod revocation;
pub mod routing;
pub mod sync_services;
pub mod sync_stacks;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use isengard_core::{Event, HostMode, Plugin, registrations_for};
use isengard_proto::FILE_DESCRIPTOR_SET;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_storage::{InsertEvent, Inventory, Journal, host::HostId};
use tokio::signal;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tracing::{info, instrument};

pub use service::ControllerService;

use crate::auth::CertAuthInterceptor;
use crate::bus::EventBus;
use crate::ca::Authority;
use crate::enrollment::EnrollmentService;
use crate::revocation::RevocationSet;

/// Bundle of references controller-side plugins need to access controller state.
/// Passed via `PluginContext.bus` (downcast from `Arc<dyn Any>`).
#[derive(Clone)]
pub struct ControllerHandles {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
    pub routing: Arc<routing::RoutingPusher>,
    /// Phase 14: enrollment-token mint/redeem service. Surfaced so the
    /// dashboard plugin can expose REST endpoints for token management.
    pub enrollment: Arc<EnrollmentService>,
    /// Phase 14: in-memory revocation set the auth interceptor reads on every
    /// RPC. Surfaced so the dashboard plugin can revoke an agent's cert via
    /// `revoke_agent` (which both writes the DB row and updates this set).
    pub revocation: RevocationSet,
    /// Phase 11a: on-disk path to the controller's SQLite file. Surfaced so
    /// the backup plugin can open its own pool for WAL checkpoint + file
    /// copy without needing a public `Inventory::pool()` getter.
    pub db_path: std::path::PathBuf,
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

    // Phase 14: internal CA + enrollment service. CA is loaded-or-initialized
    // from the `ca` row (single-row table); the EnrollmentService owns the
    // mint/redeem flow and signs leaf certs via the CA.
    let ca = Arc::new(
        Authority::load_or_init(&inventory)
            .await
            .context("loading or initializing CA")?,
    );
    let enrollment = Arc::new(EnrollmentService::new(inventory.clone(), ca.clone()));

    // Phase 14: hydrate the in-memory revocation set from the `agent_certs`
    // table so the very first RPC after boot already sees revoked certs.
    let revocation = RevocationSet::load_from_inventory(&inventory)
        .await
        .context("hydrating revocation set")?;

    // -- plugin init/start ----------------------------------------------------
    // Load + start controller-side plugins (notifier, etc).
    let handles = Arc::new(ControllerHandles {
        inventory: inventory.clone(),
        journal: journal.clone(),
        bus: bus.clone(),
        routing: routing.clone(),
        enrollment: enrollment.clone(),
        revocation: revocation.clone(),
        db_path: db_path.clone(),
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

    // Background task: subscribe to `deployment.*` events on the bus and mirror
    // the embedded Deployment row into the controller-local `deployments` table.
    // Agent-side is the source of truth; the controller's copy backs the UI list
    // queries and gets refreshed on every state-change event.
    let deployment_handler_inv = inventory.clone();
    let mut deployment_rx = bus.subscribe();
    let deployment_handle = tokio::spawn(async move {
        loop {
            match deployment_rx.recv().await {
                Ok(event) => {
                    if !event.kind.starts_with("deployment.") {
                        continue;
                    }
                    let Some(dep_value) = event.metadata.get("deployment") else {
                        tracing::warn!(
                            kind = %event.kind,
                            "deployment event missing metadata.deployment"
                        );
                        continue;
                    };
                    let dep: isengard_storage::deployment::Deployment =
                        match serde_json::from_value(dep_value.clone()) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::warn!(
                                    kind = %event.kind,
                                    error = %e,
                                    "deployment metadata parse failed"
                                );
                                continue;
                            }
                        };
                    if let Err(e) = deployment_handler_inv
                        .upsert_deployment_from_remote(&dep)
                        .await
                    {
                        tracing::warn!(
                            kind = %event.kind,
                            error = %e,
                            "upsert_deployment_from_remote failed"
                        );
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "deployment event subscriber lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // -- gRPC server ----------------------------------------------------------
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()
        .context("building reflection service")?;

    // Phase 14: per-RPC client cert validation + revocation check (replaces
    // the Phase 2c bearer-token middleware).
    let auth_layer = CertAuthInterceptor::new(revocation.clone(), ca.clone());

    // Sign the controller's own server cert with our CA. The DNS name agents
    // verify against is configurable so a single binary can be deployed under
    // different hostnames; the default matches the test fixture so dev
    // workflows don't require extra env wiring.
    let controller_dns =
        std::env::var("ISENGARD_CONTROLLER_DNS").unwrap_or_else(|_| "controller.local".into());
    // Imp-1: controller's own server cert needs both ClientAuth and
    // ServerAuth. Agents only get ClientAuth via sign_agent_leaf.
    let server_leaf = ca
        .sign_server_leaf(HostId::new(), &controller_dns, chrono::Duration::days(30))
        .context("sign controller server leaf")?;
    let identity = Identity::from_pem(
        server_leaf.cert_pem.as_bytes(),
        server_leaf.key_pem.as_bytes(),
    );
    let ca_root = Certificate::from_pem(ca.root_cert_pem().as_bytes());

    // `client_auth_optional(true)` lets the bootstrap Enroll RPC succeed
    // without a client cert. The auth layer enforces cert presence + the
    // revocation check on every other method.
    let tls_config = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(ca_root)
        .client_auth_optional(true);

    let svc = ControllerServer::new(ControllerService::new(
        inventory,
        journal,
        bus,
        routing.clone(),
        ca,
        enrollment,
        revocation,
    ));

    info!("gRPC server listening");
    let serve_fut = Server::builder()
        .tls_config(tls_config)
        .context("install server TLS config")?
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
    deployment_handle.abort();

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
