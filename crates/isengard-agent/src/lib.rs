//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

pub mod agent_state;
pub mod backoff;
pub mod container_snapshot;
pub mod enroll;
pub mod events;
pub mod labels;
pub mod proxy;
pub mod sync;
pub mod tls;

/// Convenience alias used throughout the agent.
pub type Result<T> = anyhow::Result<T>;

use std::sync::Arc;

use isengard_core::{EventEmitter, HostMode, Plugin, PluginContext, registrations_for};
use tracing::{info, instrument, warn};

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

    // -- plugin lifecycle (unchanged from Phase 1) -----------------------
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
        let ctx = PluginContext::new(HostMode::Agent, plugin_config).with_events(emitter.clone());
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // -- spawn the reverse-proxy supervisor (Phase 8b Task 10).
    //    Ports come from AgentOptions; Plan B (Phase 8e) will read them
    //    from settings. `proxy_state` is also threaded into the sync loop
    //    so Task 13's `apply_config` can mutate the upstream registry when
    //    the controller pushes a `ProxyConfig`. If both ports are None,
    //    the supervisor is not spawned (integration tests pass None to
    //    avoid fighting over 8080/8443).
    let proxy_state = proxy::ProxyState::new();
    proxy_state.set_event_sink(proxy_event_tx);

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
        proxy_state.install_cert_store(cert_store).await;
        info!(cert_dir = ?tls_opts.cert_dir, "tls: cert store installed");
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
    let (agent_msg_tx, mut agent_msg_rx) =
        tokio::sync::mpsc::channel::<isengard_proto::pb::AgentMessage>(64);
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => {
            let labels_out = agent_msg_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = labels::watch(docker, labels_out).await {
                    tracing::error!(error = %e, "labels: watcher exited");
                }
            });
        }
        Err(e) => {
            warn!(error = %e, "labels: failed to connect to Docker, watcher disabled");
        }
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
/// harmlessly. Wired into use by the renewal scheduler in PB-T12 — kept here
/// in PB-T10 so the helper is ready to consume.
#[allow(dead_code)]
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
