//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

pub mod agent_state;
pub mod backoff;
pub mod container_snapshot;
pub mod deployment;
pub mod enroll;
pub mod events;
pub mod labels;
pub mod proxy;
pub mod sync;
pub mod tls;

/// Convenience alias used throughout the agent.
pub type Result<T> = anyhow::Result<T>;

use std::sync::Arc;

use isengard_core::{
    EventEmitter, HostMode, Plugin, PluginContext, UpdateDispatcher, registrations_for,
};
use tracing::{info, instrument, warn};

use crate::deployment::{DeploymentSupervisor, SupervisorDispatcher};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller, e.g. `http://controller.example.com:9417`.
    /// Required — Phase 2d removes the `Option` wrapper from Phase 1.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
    /// Reverse-proxy ports (Phase 8b). If both are `None`, the proxy
    /// supervisor is not spawned — useful for integration tests that don't
    /// want to fight over port 8080/8443. Production passes `Some(8080)` /
    /// `Some(8443)`. Plan B will read these from settings.
    pub proxy_http_port: Option<u16>,
    pub proxy_https_port: Option<u16>,
    /// TLS subsystem options (Plan B 8e). If `None`, the cert store + HTTPS
    /// listener + ACME are all disabled — useful for integration tests.
    /// Production passes `Some(TlsOptions { ... })`.
    pub tls: Option<TlsOptions>,
}

#[derive(Debug, Clone)]
pub struct TlsOptions {
    /// Directory where issued cert/key pairs and the ACME account live
    /// (e.g. `/var/lib/isengard/tls/`). Created if missing.
    pub cert_dir: std::path::PathBuf,
    /// Contact email registered with the ACME directory on first account
    /// creation. Required by Let's Encrypt.
    pub acme_contact_email: String,
    /// ACME directory URL. Use [`tls::LE_STAGING_URL`] for development and
    /// [`tls::LE_PRODUCTION_URL`] for live issuance.
    pub acme_directory_url: String,
}

pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

#[instrument(skip(opts), fields(controller = %opts.controller_url))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(state_dir = ?opts.state_dir, "starting agent");

    // -- determine agent_id ----------------------------------------------
    let existing = agent_state::load(&opts.state_dir).await?;
    let agent_id = match existing {
        Some(state) => {
            info!(agent_id = %state.agent_id, "already enrolled, skipping enroll");
            state.agent_id
        }
        None => {
            info!("no agent.json found, enrolling with controller");
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let host_info = enroll::HostInfo::detect();
            let id = enroll::enroll(&opts.controller_url, &token, host_info).await?;
            agent_state::save(
                &opts.state_dir,
                &agent_state::AgentState {
                    agent_id: id.clone(),
                },
            )
            .await?;
            info!(agent_id = %id, "enrolled");
            id
        }
    };

    // -- build the outbound EventEmitter (mpsc-backed). Receiver lives in
    //    this scope and is passed by &mut into the sync loop so events stay
    //    queued across reconnects.
    let (emitter, mut events_rx) = events::OutboundEmitter::new();
    // Grab a sender clone before erasing the concrete type — the proxy
    // healthcheck loop needs to publish through the same channel.
    let proxy_event_tx = emitter.sender();
    let emitter: Arc<dyn EventEmitter> = Arc::new(emitter);

    // -- agent-side inventory (Phase 10): always opened so the
    //    DeploymentSupervisor can record + reconcile blue-green rows.
    //    Pre-Phase 10, only the TLS subsystem opened it (and only when TLS
    //    was enabled). The handle is cheap to clone.
    let inventory = open_agent_inventory(&opts.state_dir).await?;

    // -- shared proxy state. Constructed before plugins so the supervisor
    //    (and therefore the updater plugin's dispatcher) can hand it the
    //    same upstream registry the proxy will eventually consult.
    let proxy_state = proxy::ProxyState::new();
    proxy_state.set_event_sink(proxy_event_tx);

    // -- shared Docker handle. Used by the DeploymentSupervisor's blue-green
    //    driver and by the labels watcher below. Best-effort: if Docker is
    //    unreachable the supervisor is not built and the updater falls back
    //    to its in-place behavior. The labels watcher is also skipped.
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => Some(Arc::new(d)),
        Err(e) => {
            warn!(error = %e, "docker: connect failed, deployment supervisor + labels watcher disabled");
            None
        }
    };

    // -- DeploymentSupervisor (Phase 10 Task 7). Build it BEFORE plugins so
    //    its dispatcher can be threaded through PluginContext into the
    //    updater plugin. Reconcile orphans from a previous run before any
    //    plugin starts cycling, so we don't race the updater on a row this
    //    process is about to mark Failed.
    //
    //    `agent_id` is the controller's stringified HostId (a Ulid). Parse
    //    it back to feed both the storage HostId (for DB lookups) and the
    //    core HostId (= ulid::Ulid, surfaced to plugins via PluginContext).
    let host_ulid: ulid::Ulid = agent_id
        .parse()
        .map_err(|e| anyhow::anyhow!("agent_id {agent_id:?} is not a valid Ulid: {e}"))?;
    let storage_host_id = isengard_storage::host::HostId(host_ulid);
    let core_host_id: isengard_core::HostId = host_ulid;

    let update_dispatcher: Option<Arc<dyn UpdateDispatcher>> = match docker.as_ref() {
        Some(docker) => {
            let supervisor = Arc::new(DeploymentSupervisor::new(
                inventory.clone(),
                docker.clone(),
                proxy_state.clone(),
                emitter.clone(),
            ));
            match supervisor.reconcile_orphans(storage_host_id).await {
                Ok(0) => {}
                Ok(n) => warn!(
                    orphans = n,
                    "marked orphan deployments as failed at startup"
                ),
                Err(e) => {
                    tracing::error!(error = %e, "reconcile_orphans failed (continuing startup)")
                }
            }
            let dispatcher: Arc<dyn UpdateDispatcher> = Arc::new(SupervisorDispatcher {
                supervisor: supervisor.clone(),
                inventory: inventory.clone(),
            });
            Some(dispatcher)
        }
        None => None,
    };

    // -- plugin lifecycle ----------------------------------------------
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

    let mut started = Vec::with_capacity(plugins.len());
    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let mut ctx = PluginContext::new(HostMode::Agent, plugin_config)
            .with_events(emitter.clone())
            .with_host_id(core_host_id);
        if let Some(d) = update_dispatcher.clone() {
            ctx = ctx.with_update_dispatcher(d);
        }
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // -- TLS bring-up (Plan B Task 10). When `tls` is `Some`, install the
    //    cert_store on the ProxyState so the HTTPS listener can bind once
    //    `proxy::supervise` reads it. Production passes `Some(...)`; tests
    //    pass `None` to skip 443 binding and on-disk cert state.
    //
    //    The renewal scheduler + AcmeClient construction land in PB-T12;
    //    PB-T10 stops at installing the store.
    if let Some(tls_opts) = opts.tls.as_ref() {
        let storage = tls::TlsStorage::new(tls_opts.cert_dir.clone());
        let cert_store = std::sync::Arc::new(tls::CertStore::new(storage));
        proxy_state.install_cert_store(cert_store.clone()).await;
        info!(cert_dir = ?tls_opts.cert_dir, "tls: cert store installed");

        // Spawn the renewal scheduler. Reuses the agent-side Inventory
        // already opened above.
        let inv = std::sync::Arc::new(inventory.clone());
        let acme = std::sync::Arc::new(tls::AcmeClient::new(
            inv.clone(),
            proxy_state.acme_challenges.clone(),
            tls_opts.acme_contact_email.clone(),
            tls_opts.acme_directory_url.clone(),
        ));
        tls::renewal::spawn(inv, cert_store, acme, emitter.clone());
        info!("tls: renewal scheduler spawned");
    }

    if let (Some(http_port), Some(https_port)) = (opts.proxy_http_port, opts.proxy_https_port) {
        let ps = proxy_state.clone();
        tokio::spawn(async move {
            proxy::supervise(ps, http_port, https_port).await;
        });
    }

    // -- spawn the Docker label watcher (Phase 8b Task 16).
    //    Pushes ContainerLabelsReport / ContainerLabelsRemoved up the sync
    //    stream as containers come and go. Messages queue in `agent_msg_rx`
    //    while the stream is down; the sync loop drains them on reconnect.
    //    Reuses the shared `docker` handle opened earlier.
    let (agent_msg_tx, mut agent_msg_rx) =
        tokio::sync::mpsc::channel::<isengard_proto::pb::AgentMessage>(64);
    if let Some(docker) = docker.as_ref() {
        let labels_docker = (**docker).clone();
        let labels_out = agent_msg_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = labels::watch(labels_docker, labels_out).await {
                tracing::error!(error = %e, "labels: watcher exited");
            }
        });
    }

    // -- run sync loop in background; ctrl_c triggers shutdown ----------
    let token = std::env::var("ISENGARD_TOKEN")
        .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

    // Phase 2e hardcodes 10s heartbeat interval. Phase 2e+ honors
    // heartbeat_interval_secs from EnrollResponse.
    const HEARTBEAT_INTERVAL_SECS: u32 = 10;

    // The sync loop borrows `events_rx` for its lifetime — own it here.
    let sync_url = opts.controller_url.clone();
    let sync_agent_id = agent_id.clone();
    let sync_cancel = cancel.clone();
    let sync_proxy_state = proxy_state.clone();
    let sync_fut = async move {
        sync::run_sync_with_reconnect(
            sync_url,
            token,
            sync_agent_id,
            HEARTBEAT_INTERVAL_SECS,
            sync_cancel,
            &mut events_rx,
            &mut agent_msg_rx,
            sync_proxy_state,
        )
        .await
    };
    tokio::pin!(sync_fut);

    // Wait for ctrl_c OR for the sync loop to terminate on its own.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl_c received, shutting down");
            cancel.notify_waiters();

            // Wait for sync to wind down (with a timeout so we don't hang forever).
            let sync_result =
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut sync_fut).await;
            match sync_result {
                Ok(Ok(())) => info!("sync loop exited cleanly"),
                Ok(Err(e)) => warn!(error = %e, "sync loop exited with error"),
                Err(_) => warn!("sync loop timed out on shutdown"),
            }
        }
        res = &mut sync_fut => {
            match res {
                Ok(()) => info!("sync loop exited cleanly before ctrl_c"),
                Err(e) => warn!(error = %e, "sync loop exited with error before ctrl_c"),
            }
        }
    }

    // -- plugin stop ---------------------------------------------------
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!(agent_id = %agent_id, "agent exited cleanly");
    Ok(())
}

/// Open (creating if missing) the agent-side `Inventory` SQLite database at
/// `<state_dir>/agent.db`. Plan B introduces an agent-side inventory used by
/// the TLS subsystem (`tls_certs`, `acme_account`); other tables apply
/// harmlessly. Consumed by the renewal scheduler spawned in `run_agent`.
async fn open_agent_inventory(state_dir: &std::path::Path) -> Result<isengard_storage::Inventory> {
    let db_path = state_dir.join("agent.db");
    isengard_storage::Inventory::open(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("opening agent inventory: {e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn dummy() {
        // placeholder; real tests come in Tasks 2-4 of Phase 2d
        assert_eq!(2 + 2, 4);
    }
}
