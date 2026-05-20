#![doc = include_str!("../docs/crate-overview.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod agent_labels;
pub mod agent_state;
pub mod backoff;
pub mod cert_renewal;
pub mod cert_store;
pub mod compose_apply;
pub mod compose_export;
pub mod compose_import;
pub mod compose_reconciler;
pub mod compose_watcher;
pub mod compose_writer;
pub mod container_snapshot;
pub mod deployment;
pub mod enroll;
pub mod enroll_diagnosis;
pub mod events;
pub mod labels;
pub mod lifecycle_hooks;
pub mod logs;
pub mod mdns;
pub mod placement;
pub mod proxy;
pub mod runtime;
pub mod secret_fetch;
pub mod stack_secrets;
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

/// Boot-time configuration the agent reads to come up. Built by the
/// `isd join` / `isd agent` CLI handlers from flags + config files.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller, e.g. `https://controller.example.com:9417`.
    /// Required. Must be `https://` since the gRPC server is mTLS-only.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` +
    /// `certs/{ca,agent.crt,agent.key}` etc). Created if missing.
    pub state_dir: std::path::PathBuf,
    /// Plugin-specific configuration, keyed by plugin name. Each plugin
    /// reads its own subtree from this `Value` at `init` time.
    pub config: serde_json::Value,
    /// Reverse-proxy ports. If both are `None`, the proxy
    /// supervisor is not spawned: useful for integration tests that don't
    /// want to fight over port 8080/8443. Production passes `Some(8080)` /
    /// `Some(8443)`. Plan B will read these from settings.
    pub proxy_http_port: Option<u16>,
    /// HTTPS port for the Pingora listener. See [`Self::proxy_http_port`].
    pub proxy_https_port: Option<u16>,
    /// TLS subsystem options (Plan B 8e). If `None`, the cert store + HTTPS
    /// listener + ACME are all disabled: useful for integration tests.
    /// Production passes `Some(TlsOptions { ... })`.
    pub tls: Option<TlsOptions>,
    /// One-time enrollment token. Used only on first boot when
    /// `agent.json` does not yet exist. The runtime also falls back to the
    /// `ISENGARD_ENROLL_TOKEN` env var. Once an agent has a persisted cert
    /// bundle, this is ignored.
    pub enroll_token: Option<String>,
    /// Optional trust pin for the bootstrap Enroll RPC.
    /// Required when the controller serves a self-signed (internal-CA) cert,
    /// which is the default. Resolution order in [`enroll::enroll`]:
    /// `ISENGARD_CONTROLLER_CA_PEM_PATH` env > `ISENGARD_CONTROLLER_CA_PEM`
    /// env > this struct's path > this struct's inline PEM > native roots.
    pub bootstrap_trust: enroll::BootstrapTrust,
    /// Optional name of the network interface the mDNS responder should
    /// advertise on (v0.3a). When `None`, the responder picks the first
    /// non-loopback IPv4 interface. When the chosen interface lacks a
    /// non-loopback IPv4, the responder is not started and the agent logs
    /// a warning: routing still works, without `.local` advertisement.
    pub advertise_iface: Option<String>,
}

/// TLS subsystem options. Production wiring fills this from the agent's
/// settings; tests pass `None` on [`AgentOptions::tls`] to skip the
/// subsystem entirely.
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

/// Construct every agent-mode plugin registered via `inventory`. The
/// returned vector preserves registration order; the boot path calls
/// `Plugin::init` then `Plugin::start` on each entry.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Agent entry point. Runs until SIGINT or the sync loop returns.
///
/// On first boot the agent enrolls against the controller (fingerprint-
/// verified CA + `Enroll` RPC), persists the cert bundle, and brings up
/// the sync stream + proxy + label watcher. Subsequent boots load
/// `agent.json` and re-attach without a fresh enroll.
///
/// # Errors
///
/// Returns `Err` for any unrecoverable boot failure (missing state dir,
/// enroll token rejected, on-disk state corrupt). Once the sync loop is
/// running, transient stream errors are absorbed by the reconnect loop;
/// only `cancel` or an unrecoverable plugin error tears the agent down.
#[instrument(skip(opts), fields(controller = %opts.controller_url))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(state_dir = ?opts.state_dir, "starting agent");

    // kill-fleets (0.15): the fleet concept has been removed. Existing
    // installs that set `ISENGARD_FLEET` get a one-time warning so the
    // operator can migrate to host labels (agent.toml `[labels]` /
    // placement selectors `where = "fleet==lab"`). The env var is
    // otherwise ignored. Drop this warning in the next release.
    if std::env::var("ISENGARD_FLEET").is_ok() {
        tracing::warn!(
            "ISENGARD_FLEET is set but the fleet concept has been removed. \
            Use host labels via /etc/isengard/agent.toml `[labels]` and \
            placement selectors instead. This warning will be removed in \
            the next release."
        );
    }

    // Make sure state_dir exists before the cert store / agent.json writes
    // hit it. The CLI also does this, but other entry points (tests) may not.
    std::fs::create_dir_all(&opts.state_dir)
        .map_err(|e| anyhow::anyhow!("creating state dir {:?}: {e}", opts.state_dir))?;

    // -- determine agent_id + cert bundle --------------------------------
    //    First boot reads ISENGARD_ENROLL_TOKEN (or
    //    AgentOptions::enroll_token), exchanges it for a signed cert bundle
    //    via Enroll, and persists both the bundle and agent.json. Subsequent
    //    boots refuse to start if agent.json exists without certs.
    let (agent_id, heartbeat_interval_secs) = match agent_state::load(&opts.state_dir).await? {
        Some(state) => {
            if !cert_store::exists(&opts.state_dir) {
                anyhow::bail!(
                    "agent.json present but no cert bundle in {}/certs: wipe state and re-enroll",
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
                    "cert bundle present in {}/certs but no agent.json: wipe certs/ and re-enroll",
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

            // The join token MUST be in the packed format
            // (`TK<base32-bytes>.<base32-fingerprint>`). The agent
            // fetches the controller's CA via skip-verify TLS, checks
            // its SHA-256 against the fingerprint embedded in the
            // token, and pins the verified PEM as the bootstrap trust
            // root for the real Enroll RPC. A non-packed token is a
            // hard error: there are no env-var CA fallbacks.
            if isengard_core::join_token::parse(&enroll_token).is_err() {
                return Err(anyhow::anyhow!(
                    "enrollment token is in a legacy format and cannot verify the controller CA. \
                     Mint a fresh token with `isd join-token` and re-run `isd join`."
                ));
            }
            let pem = enroll::fetch_and_verify_ca(&opts.controller_url, &enroll_token).await?;
            info!("enroll: fingerprint-verified controller CA via packed token");
            let bootstrap_trust = enroll::BootstrapTrust {
                verified_ca_pem: Some(pem),
            };

            let outcome = match enroll::enroll(
                &opts.controller_url,
                &enroll_token,
                host_info,
                bootstrap_trust,
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
    //    `connect()` calls: sync's reconnect loop dials per attempt.
    //
    //    Imp-2: wrap the Endpoint in `Arc<RwLock<_>>` so the cert renewal
    //    task can swap in a freshly-built Endpoint after rotating the
    //    on-disk cert. Sync's reconnect loop reads this on every attempt
    //    and adopts the new identity at the next stream cycle: without a
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
    // Grab a sender clone before erasing the concrete type: the proxy
    // healthcheck loop needs to publish through the same channel.
    let proxy_event_tx = emitter.sender();
    let emitter: Arc<dyn EventEmitter> = Arc::new(emitter);

    // -- agent-side inventory: always opened so the
    //    DeploymentSupervisor can record + reconcile blue-green rows.
    //    Previously, only the TLS subsystem opened it (and only when TLS
    //    was enabled). The handle is cheap to clone.
    let inventory = open_agent_inventory(&opts.state_dir).await?;

    // -- shared proxy state. Constructed before plugins so the supervisor
    //    (and therefore the updater plugin's dispatcher) can hand it the
    //    same upstream registry the proxy will eventually consult.
    let proxy_state = proxy::ProxyState::new();
    proxy_state.set_event_sink(proxy_event_tx);

    // -- shared runtime backend. Docker is the only choice; the trait
    //    is kept as the public seam (deeply-bollard helpers in
    //    compose_apply, deployment/driver, labels, proxy/discovery still
    //    reach for the underlying bollard handle via
    //    [`runtime::RuntimeBackend::as_bollard`]; lifting them onto the
    //    trait is a follow-up cleanup).
    //    Best-effort: if backend construction fails the supervisor + labels
    //    watcher are disabled (matches pre-0.4 behavior when docker.sock
    //    was unreachable).
    let backend: Option<Arc<dyn runtime::RuntimeBackend>> =
        match runtime::bollard_backend::BollardBackend::from_env(&opts.state_dir).await {
            Ok(b) => {
                tracing::info!("runtime backend: docker (bollard)");
                Some(Arc::new(b) as Arc<dyn runtime::RuntimeBackend>)
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "runtime: bollard backend init failed, deployment supervisor + labels watcher disabled",
                );
                None
            }
        };

    // Legacy bollard-typed handle for callers that haven't been fully
    // ported to the RuntimeBackend trait (compose_apply,
    // deployment/driver::RealDriverDeps, the labels watcher, the bollard
    // log source, proxy/discovery). Docker is now the only backend so
    // the accessor is effectively an identity; kept until the call sites
    // get rewritten.
    let docker = backend.as_ref().and_then(|b| b.as_bollard());

    // -- DeploymentSupervisor. Build it BEFORE plugins so
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

    // -- cert renewal task. Polls the on-disk cert TTL and
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

    // Build a `PolicyLoader` backed by the agent inventory so the
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
    // Build an `ApprovalStore` backed by the same inventory so the
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

        // Pre-populate the cache from disk so the very first SNI handshake
        // after boot has the cert ready, without waiting for the
        // controller to push a `ProxyConfig`. Crucial for wildcard certs:
        // disk-only fallback would otherwise miss because lookup is
        // hostname-keyed but wildcard files are stored under their
        // encoded `_wildcard_.` form.
        match cert_store.hydrate().await {
            Ok(0) => info!(cert_dir = ?tls_opts.cert_dir, "tls: cert store empty at boot"),
            Ok(n) => info!(
                cert_dir = ?tls_opts.cert_dir,
                count = n,
                "tls: cert store hydrated from disk",
            ),
            Err(e) => warn!(error = %e, "tls: cert store hydrate failed; starting empty"),
        }

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

    // -- spawn the runtime label watcher (Task 16, trait-ified
    // ).
    //    Pushes ContainerLabelsReport / ContainerLabelsRemoved up the sync
    //    stream as containers come and go. Messages queue in `agent_msg_rx`
    //    while the stream is down; the sync loop drains them on reconnect.
    //    Drives off `RuntimeBackend::stream_events` + `inspect_container`
    //    so wisp can supply Pingora label discovery on the same path as
    //    bollard.
    let (agent_msg_tx, mut agent_msg_rx) =
        tokio::sync::mpsc::channel::<isengard_proto::pb::AgentMessage>(64);
    if let Some(backend) = backend.as_ref() {
        let labels_backend = backend.clone();
        let labels_out = agent_msg_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = labels::watch(labels_backend, labels_out).await {
                tracing::error!(error = %e, "labels: watcher exited");
            }
        });

        // v0.3c compose import: sweep `isengard.enable=true` stacks and
        // write a reverse-engineered `compose.yaml` to disk + report it
        // to the controller. Mounted at /etc/isengard/stacks via the
        // bind in docker/compose.yaml so the file persists across
        // container restarts.
        //
        // Still bollard-coupled (reverse-engineering compose.yaml
        // from running containers reaches into bollard inspect details the
        // trait doesn't surface yet). Gated on the bollard escape hatch;
        // fresh wisp installs skip compose-import: the use case is
        // migrating dockerd-installed homelabs and isn't applicable there.
        let import_root = std::path::PathBuf::from(
            std::env::var("ISENGARD_COMPOSE_IMPORT_ROOT")
                .unwrap_or_else(|_| compose_import::DEFAULT_IMPORT_ROOT.to_string()),
        );
        if let Some(docker) = docker.as_ref() {
            compose_import::spawn(
                docker.clone(),
                agent_msg_tx.clone(),
                agent_id.clone(),
                import_root.clone(),
                compose_import::DEFAULT_IMPORT_INTERVAL,
            );
        }

        // v0.3d compose-as-truth: watch the same root for operator
        // edits (vim, git pull, dashboard PUT). Each debounced change
        // drives a reconcile sweep against the running containers.
        //
        // Lifted out of the if-let-Some-docker gate so wisp
        // deploys also see the watcher. reconcile_stack_with_secrets
        // drives the RuntimeBackend trait directly; the watcher only
        // captures an Arc<dyn RuntimeBackend>, no bollard borrow.
        let watcher_backend = backend.clone();
        let watcher_root = import_root.clone();
        let watcher_endpoint = endpoint.clone();
        match compose_watcher::spawn(watcher_root.clone()) {
            Ok((mut rx, watcher)) => {
                // Hold the watcher Arc so it isn't dropped (which would
                // stop the underlying inotify/FSEvents subscription).
                tokio::spawn(async move {
                    let _watcher_keepalive = watcher;
                    while let Some(evt) = rx.recv().await {
                        let yaml = match std::fs::read_to_string(&evt.path) {
                            Ok(y) => y,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    path = %evt.path.display(),
                                    "compose_watcher: read after settle failed",
                                );
                                continue;
                            }
                        };
                        let new_sha = compose_writer::sha256_hex(&yaml);
                        // Skip reconcile when the on-disk hash matches
                        // the agent's own last_applied_hash (we caused
                        // this event ourselves via write_compose). The
                        // first watcher event after our own write is
                        // therefore a noop, which keeps recreate from
                        // looping on its own writes.
                        let stack_dir = watcher_root.join(&evt.stack_name);
                        if compose_writer::matches_last_applied(&stack_dir, &new_sha)
                            .unwrap_or(false)
                        {
                            tracing::debug!(
                                stack = %evt.stack_name,
                                "compose_watcher: change matches last_applied, skipping reconcile",
                            );
                            continue;
                        }
                        tracing::info!(
                            stack = %evt.stack_name,
                            sha256 = %new_sha,
                            "compose_watcher: file changed, starting reconcile",
                        );
                        // v0.3.6: when the compose declares external
                        // secrets, route through the variant that
                        // fetches + tmpfs-mounts them. We always pass the
                        // endpoint; the variant only contacts the
                        // controller when at least one service has
                        // `secrets:` set OR the stack.toml declares
                        // `secrets = [.]` at fleet level (
                        // follow-up).
                        //
                        // Stack-level secrets are read off the persisted
                        // `<stack_dir>/stack.toml`. A read failure
                        // (malformed array, IO error) aborts the
                        // reconcile rather than mounting an ambiguous
                        // set: the operator's intent isn't clear enough
                        // to deploy safely.
                        let stack_level_secrets = match stack_secrets::read_stack_secrets(
                            &stack_dir,
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    stack = %evt.stack_name,
                                    "compose_watcher: failed to read stack-level secrets; skipping reconcile",
                                );
                                continue;
                            }
                        };
                        match compose_apply::reconcile_stack_with_stack_secrets(
                            watcher_backend.as_ref(),
                            &evt.stack_name,
                            &yaml,
                            watcher_endpoint.clone(),
                            &stack_level_secrets,
                        )
                        .await
                        {
                            Ok((plan, outcomes)) => {
                                let failed = outcomes.iter().filter(|o| o.error.is_some()).count();
                                if plan.is_noop() {
                                    tracing::debug!(
                                        stack = %evt.stack_name,
                                        "compose_watcher: reconcile noop",
                                    );
                                } else {
                                    tracing::info!(
                                        stack = %evt.stack_name,
                                        ops = plan.ops.len(),
                                        failed = failed,
                                        "compose_watcher: reconcile applied",
                                    );
                                }
                                // Emit per-op errors so operators can see WHY
                                // a reconcile failed without bumping the
                                // whole agent to debug. The summary's
                                // `failed=N` is a counter, not actionable.
                                for outcome in &outcomes {
                                    if let Some(err) = outcome.error.as_ref() {
                                        tracing::warn!(
                                            stack = %evt.stack_name,
                                            service = %outcome.op.service(),
                                            error = %err,
                                            "compose_watcher: op failed",
                                        );
                                    }
                                }
                                if failed == 0 {
                                    if let Err(e) =
                                        compose_writer::record_last_applied(&stack_dir, &new_sha)
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            stack = %evt.stack_name,
                                            "compose_watcher: record_last_applied failed",
                                        );
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                stack = %evt.stack_name,
                                "compose_watcher: reconcile failed",
                            ),
                        }
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "compose_watcher: failed to spawn (continuing without)")
            }
        }
    }

    // -- mDNS responder (v0.3a). The advertise IP is the chosen interface's
    //    first non-loopback IPv4. Failures are non-fatal: the agent keeps
    //    serving traffic, without `.local` advertisement until the
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

    // Optional log source for `StartLogStream` subscriptions.
    // Same `docker` handle as the labels watcher and the deployment driver;
    // tests / docker-less envs skip log streaming the same way they skip
    // those subsystems.
    let log_source = docker
        .as_ref()
        .map(|d| Arc::new(crate::logs::BollardLogSource { docker: d.clone() }));

    // The sync loop borrows `events_rx` for its lifetime: own it here.
    let sync_endpoint = endpoint.clone();
    let sync_agent_id = agent_id.clone();
    let sync_cancel = cancel.clone();
    let sync_proxy_state = proxy_state.clone();
    let sync_supervisor = supervisor_for_sync.clone();
    let sync_log_source = log_source.clone();
    let sync_mdns = mdns_handle.clone();
    let sync_backend = backend.clone();
    // Load agent labels once at boot. SIGHUP re-read is
    // future work; for now the loader runs at agent start and the same
    // map travels with every heartbeat. BTreeMap from the loader is
    // converted to HashMap because the prost-generated `Heartbeat.labels`
    // field is a HashMap.
    let agent_label_map: std::collections::HashMap<String, String> =
        agent_labels::load_agent_labels().into_iter().collect();
    if !agent_label_map.is_empty() {
        tracing::info!(
            count = agent_label_map.len(),
            "agent_labels: loaded from agent.toml + env",
        );
    }
    let sync_agent_labels = agent_label_map;
    // v0.3d: surface the compose root + host id to the sync loop so it
    // can service `WriteCompose` ControllerMessages from the dashboard.
    // Also surface the EventEmitter so the
    // lifecycle-hook executor can publish `lifecycle_hook.*` audit
    // events back to the controller via the existing outbound channel.
    let sync_compose_ctx = Some(sync::ComposeContext {
        root: std::path::PathBuf::from(
            std::env::var("ISENGARD_COMPOSE_IMPORT_ROOT")
                .unwrap_or_else(|_| compose_import::DEFAULT_IMPORT_ROOT.to_string()),
        ),
        host_id: agent_id.clone(),
        event_emitter: Some(emitter.clone()),
    });
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
            sync_compose_ctx,
            sync_backend,
            sync_agent_labels,
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
/// Get this value from the EnrollResponse.
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u32 = 10;

/// Build the reusable mTLS [`tonic::transport::Endpoint`] every post-enroll
/// RPC dials through. The endpoint is cheap to clone and each `connect()`
/// call produces a fresh transport: sync's reconnect loop relies on this.
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
/// Tiny manual parser: pulling in `url` for one call wasn't worth it.
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
        // host[:port]: drop the port if present.
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
