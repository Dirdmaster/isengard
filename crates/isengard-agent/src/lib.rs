//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

pub mod agent_state;
pub mod backoff;
pub mod cert_renewal;
pub mod cert_store;
pub mod container_snapshot;
pub mod deployment;
pub mod enroll;
pub mod enroll_diagnosis;
pub mod events;
pub mod labels;
pub mod logs;
pub mod mdns;
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
    /// URL of the controller, e.g. `https://controller.example.com:9417`.
    /// Required — Phase 2d removes the `Option` wrapper from Phase 1. Must be
    /// `https://` post-Phase-14 since the gRPC server is mTLS-only.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` +
    /// `certs/{ca,agent.crt,agent.key}` etc). Created if missing.
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
    /// One-time enrollment token (Phase 14). Used only on first boot when
    /// `agent.json` does not yet exist. The runtime also falls back to the
    /// `ISENGARD_ENROLL_TOKEN` env var. Once an agent has a persisted cert
    /// bundle, this is ignored.
    pub enroll_token: Option<String>,
    /// Optional trust pin for the bootstrap Enroll RPC (Phase 14 task 15).
    /// Required when the controller serves a self-signed (internal-CA) cert,
    /// which is the default. Resolution order in [`enroll::enroll`]:
    /// `ISENGARD_CONTROLLER_CA_PEM_PATH` env > `ISENGARD_CONTROLLER_CA_PEM`
    /// env > this struct's path > this struct's inline PEM > native roots.
    pub bootstrap_trust: enroll::BootstrapTrust,
    /// Optional name of the network interface the mDNS responder should
    /// advertise on (v0.3a). When `None`, the responder picks the first
    /// non-loopback IPv4 interface. When the chosen interface lacks a
    /// non-loopback IPv4, the responder is not started and the agent logs
    /// a warning: routing still works, just without `.local` advertisement.
    pub advertise_iface: Option<String>,
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

    // Make sure state_dir exists before the cert store / agent.json writes
    // hit it. The CLI also does this, but other entry points (tests) may not.
    std::fs::create_dir_all(&opts.state_dir)
        .map_err(|e| anyhow::anyhow!("creating state dir {:?}: {e}", opts.state_dir))?;

    // -- determine agent_id + cert bundle --------------------------------
    //    Phase 14: first boot reads ISENGARD_ENROLL_TOKEN (or
    //    AgentOptions::enroll_token), exchanges it for a signed cert bundle
    //    via Enroll, and persists both the bundle and agent.json. Subsequent
    //    boots refuse to start if agent.json exists without certs.
    let (agent_id, heartbeat_interval_secs) = match agent_state::load(&opts.state_dir).await? {
        Some(state) => {
            if !cert_store::exists(&opts.state_dir) {
                anyhow::bail!(
                    "agent.json present but no cert bundle in {}/certs — wipe state and re-enroll",
                    opts.state_dir.display(),
                );
            }
            info!(agent_id = %state.agent_id, "already enrolled, skipping enroll");
            let hb = state
                .heartbeat_interval_secs
                .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_SECS);
            (state.agent_id, hb)
        }
        None => {
            // Imp-5 fix: symmetric to the agent.json-without-certs branch
            // above. If the cert bundle is on disk but agent.json isn't,
            // the previous boot crashed mid-enroll or someone partially
            // restored state; either way we'd silently re-enroll and end
            // up with a second cert pointing at a different HostId. Bail
            // and ask the operator to clean up.
            if cert_store::exists(&opts.state_dir) {
                anyhow::bail!(
                    "cert bundle present in {}/certs but no agent.json — wipe certs/ and re-enroll",
                    opts.state_dir.display(),
                );
            }
            info!("no agent.json found, enrolling with controller");
            let enroll_token = std::env::var("ISENGARD_ENROLL_TOKEN")
                .ok()
                .or_else(|| opts.enroll_token.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "ISENGARD_ENROLL_TOKEN env var (or --enroll-token) required for \
                         first-time enrollment. Mint one with `isengard controller token mint`."
                    )
                })?;
            let host_info = enroll::HostInfo::detect();
            let outcome = match enroll::enroll(
                &opts.controller_url,
                &enroll_token,
                host_info,
                opts.bootstrap_trust.clone(),
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    // Polish: pattern-match common ops-level enroll failures
                    // into a friendly stderr block. The full anyhow chain is
                    // preserved at debug level so `RUST_LOG=debug` still gets
                    // the underlying tonic/h2/transport detail.
                    if let Some(d) = enroll_diagnosis::diagnose(&e) {
                        eprintln!("{}", enroll_diagnosis::render(d, &opts.controller_url));
                        tracing::debug!(error = ?e, "enrollment failed: full error chain");
                    }
                    return Err(e);
                }
            };
            // Drop the plaintext token from memory once the bundle is in hand.
            drop(enroll_token);

            cert_store::save(&opts.state_dir, &outcome.bundle)
                .map_err(|e| anyhow::anyhow!("persisting cert bundle: {e}"))?;
            let agent_id_str = outcome.host_id.to_string();
            agent_state::save(
                &opts.state_dir,
                &agent_state::AgentState {
                    agent_id: agent_id_str.clone(),
                    controller_url: Some(opts.controller_url.clone()),
                    heartbeat_interval_secs: Some(outcome.heartbeat_interval_secs),
                },
            )
            .await?;
            info!(agent_id = %agent_id_str, "enrolled");
            (agent_id_str, outcome.heartbeat_interval_secs)
        }
    };

    // -- build the mTLS endpoint reused by the sync stream + any future
    //    direct RPC the agent issues (RenewCert, etc). One Endpoint, many
    //    `connect()` calls — sync's reconnect loop dials per attempt.
    //
    //    Imp-2: wrap the Endpoint in `Arc<RwLock<_>>` so the cert renewal
    //    task can swap in a freshly-built Endpoint after rotating the
    //    on-disk cert. Sync's reconnect loop reads this on every attempt
    //    and adopts the new identity at the next stream cycle — without a
    //    restart.
    let bundle = cert_store::load(&opts.state_dir)
        .map_err(|e| anyhow::anyhow!("loading cert bundle: {e}"))?;
    let initial_endpoint = build_mtls_endpoint(&opts.controller_url, &bundle)?;
    let endpoint: Arc<tokio::sync::RwLock<tonic::transport::Endpoint>> =
        Arc::new(tokio::sync::RwLock::new(initial_endpoint));

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

    // -- Phase 14 Task 12: cert renewal task. Polls the on-disk cert TTL and
    //    swaps in a fresh bundle once past 50%. Imp-2: the task and the sync
    //    loop share an `Arc<RwLock<Endpoint>>`; on renewal the task rebuilds
    //    the Endpoint from the new bundle and replaces the inner. Sync's
    //    reconnect loop reads the current Endpoint on every attempt, so the
    //    new cert propagates without restarting the agent.
    {
        let renewal_endpoint = endpoint.clone();
        let renewal_state_dir = opts.state_dir.clone();
        let renewal_url = opts.controller_url.clone();
        let endpoint_builder: cert_renewal::EndpointBuilder =
            std::sync::Arc::new(build_mtls_endpoint);
        tokio::spawn(async move {
            if let Err(e) = cert_renewal::run_renewal_loop(
                renewal_state_dir,
                storage_host_id,
                renewal_endpoint,
                renewal_url,
                endpoint_builder,
                std::time::Duration::from_secs(60),
            )
            .await
            {
                warn!(error = %e, "cert_renewal: loop exited");
            }
        });
    }

    // Phase 9b: build a `PolicyLoader` backed by the agent inventory so the
    // updater plugin can consult the policies table once per cycle. Phase
    // 9F: also passed into the supervisor so it can resolve `on_failure`
    // and seed `previous_digest` for Rollback-policy deployments.
    let policy_loader: Arc<dyn isengard_core::PolicyLoader> = Arc::new(
        isengard_storage::InventoryPolicyLoader::new(Arc::new(inventory.clone())),
    );

    // Hold onto the supervisor `Arc` so the sync loop can dispatch
    // `AbortDeployment` payloads into [`DeploymentSupervisor::handle_abort`].
    // The dispatcher path consumes a clone via `SupervisorDispatcher`; the
    // `supervisor_for_sync` clone is the second consumer.
    let mut supervisor_for_sync: Option<Arc<DeploymentSupervisor>> = None;
    let update_dispatcher: Option<Arc<dyn UpdateDispatcher>> = match docker.as_ref() {
        Some(docker) => {
            let supervisor = Arc::new(
                DeploymentSupervisor::new(
                    inventory.clone(),
                    docker.clone(),
                    proxy_state.clone(),
                    emitter.clone(),
                )
                .with_policy_loader(policy_loader.clone()),
            );
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
            supervisor_for_sync = Some(supervisor);
            Some(dispatcher)
        }
        None => None,
    };

    // -- plugin lifecycle ----------------------------------------------
    //
    // Phase 9e: build an `ApprovalStore` backed by the same inventory so the
    // updater can persist + dedupe pending-approval rows when a candidate's
    // resolved policy gates on `Approval`.
    let approval_store: Arc<dyn isengard_core::ApprovalStore> = Arc::new(
        isengard_storage::InventoryApprovalStore::new(Arc::new(inventory.clone())),
    );

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
            .with_host_id(core_host_id)
            .with_policy_loader(policy_loader.clone())
            .with_approval_store(approval_store.clone());
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

    // -- mDNS responder (v0.3a). The advertise IP is the chosen interface's
    //    first non-loopback IPv4. Failures are non-fatal: the agent keeps
    //    serving traffic, you just don't get `.local` advertisement until the
    //    interface comes back. The responder advertises on the same port the
    //    HTTP listener bound (defaults to 80 when proxy_http_port is None,
    //    matching the production compose setup).
    let mdns_handle: Option<sync::MdnsHandle> =
        match mdns::pick_advertise_ip(opts.advertise_iface.as_deref()) {
            Ok(ip) => {
                let advertise_port = opts.proxy_http_port.unwrap_or(mdns::DEFAULT_HTTP_PORT);
                match mdns::MdnsResponder::new(ip, advertise_port) {
                    Ok(resp) => Some(Arc::new(tokio::sync::Mutex::new(resp))),
                    Err(e) => {
                        warn!(error = %e, "mdns: responder disabled (continuing)");
                        None
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "mdns: cannot pick advertise interface (continuing without)");
                None
            }
        };

    // -- run sync loop in background; ctrl_c triggers shutdown ----------
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

    // Phase 13B: optional log source for `StartLogStream` subscriptions.
    // Same `docker` handle as the labels watcher and the deployment driver;
    // tests / docker-less envs skip log streaming the same way they skip
    // those subsystems.
    let log_source = docker
        .as_ref()
        .map(|d| Arc::new(crate::logs::BollardLogSource { docker: d.clone() }));

    // The sync loop borrows `events_rx` for its lifetime — own it here.
    let sync_endpoint = endpoint.clone();
    let sync_agent_id = agent_id.clone();
    let sync_cancel = cancel.clone();
    let sync_proxy_state = proxy_state.clone();
    let sync_supervisor = supervisor_for_sync.clone();
    let sync_log_source = log_source.clone();
    let sync_mdns = mdns_handle.clone();
    let sync_fut = async move {
        sync::run_sync_with_reconnect(
            sync_endpoint,
            sync_agent_id,
            heartbeat_interval_secs,
            sync_cancel,
            &mut events_rx,
            &mut agent_msg_rx,
            sync_proxy_state,
            sync_supervisor,
            sync_log_source,
            sync_mdns,
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

/// Fallback heartbeat interval when the persisted `agent.json` predates Phase
/// 14 (no `heartbeat_interval_secs` field). Production agents enrolled on
/// Phase 14+ get this value from the EnrollResponse.
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u32 = 10;

/// Build the reusable mTLS [`tonic::transport::Endpoint`] every post-enroll
/// RPC dials through. The endpoint is cheap to clone and each `connect()`
/// call produces a fresh transport — sync's reconnect loop relies on this.
fn build_mtls_endpoint(
    controller_url: &str,
    bundle: &cert_store::CertBundle,
) -> Result<tonic::transport::Endpoint> {
    let dns = controller_dns_from_url(controller_url)
        .ok_or_else(|| anyhow::anyhow!("controller_url {controller_url:?} has no host part"))?;
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(
            bundle.ca_pem.as_bytes(),
        ))
        .identity(tonic::transport::Identity::from_pem(
            bundle.cert_pem.as_bytes(),
            bundle.key_pem.as_bytes(),
        ))
        .domain_name(dns);
    let endpoint = tonic::transport::Endpoint::from_shared(controller_url.to_string())
        .map_err(|e| anyhow::anyhow!("invalid controller_url {controller_url:?}: {e}"))?
        .tls_config(tls)
        .map_err(|e| anyhow::anyhow!("install agent mTLS config: {e}"))?;
    Ok(endpoint)
}

/// Extract the DNS host from a controller URL, e.g.
/// `https://controller.local:9417/foo` → `Some("controller.local")`.
/// Used as the SNI / cert-verification name for the mTLS channel.
///
/// Tiny manual parser — pulling in `url` for one call wasn't worth it.
fn controller_dns_from_url(url: &str) -> Option<String> {
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let after_userinfo = match after_scheme.split_once('@') {
        Some((_, rest)) => rest,
        None => after_scheme,
    };
    let host_with_port = after_userinfo
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_userinfo);
    let host = if let Some(rest) = host_with_port.strip_prefix('[') {
        // IPv6 literal: take everything before the closing bracket.
        rest.split(']').next()?.to_string()
    } else {
        // host[:port] — drop the port if present.
        host_with_port
            .split_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| host_with_port.to_string())
    };
    if host.is_empty() { None } else { Some(host) }
}

#[cfg(test)]
mod tests {
    use super::controller_dns_from_url;

    #[test]
    fn dns_from_url_strips_scheme_and_port() {
        assert_eq!(
            controller_dns_from_url("https://controller.local:9417").as_deref(),
            Some("controller.local"),
        );
    }

    #[test]
    fn dns_from_url_strips_path() {
        assert_eq!(
            controller_dns_from_url("https://controller.example.com:9417/foo/bar").as_deref(),
            Some("controller.example.com"),
        );
    }

    #[test]
    fn dns_from_url_handles_no_scheme() {
        assert_eq!(
            controller_dns_from_url("controller.local:9417").as_deref(),
            Some("controller.local"),
        );
    }

    #[test]
    fn dns_from_url_handles_userinfo() {
        assert_eq!(
            controller_dns_from_url("https://user:pass@controller.local:9417").as_deref(),
            Some("controller.local"),
        );
    }

    #[test]
    fn dns_from_url_handles_ipv6_literal() {
        assert_eq!(
            controller_dns_from_url("https://[::1]:9417").as_deref(),
            Some("::1"),
        );
    }

    #[test]
    fn dns_from_url_returns_none_for_empty() {
        assert_eq!(controller_dns_from_url("https://").as_deref(), None);
    }
}
