//! Controller-mode runtime: load plugins, bind the gRPC server, await
//! shutdown, drain, stop plugins, exit.

mod service;

pub mod acme;
pub mod auth;
pub mod bus;
pub mod ca;
pub mod compose_broker;
pub mod container_id;
pub mod disconnect_monitor;
pub mod dns;
pub mod enrollment;
pub mod log_fanout;
pub mod pending_actions;
pub mod plugin_host;
pub mod policy_ingest;
pub mod revocation;
pub mod routing;
pub mod secrets;
pub mod stack_deploy_orchestrator;
pub mod sync_containers;
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
use tracing::{info, instrument, warn};

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
    /// Phase 13B: subscription registry for log streaming. Dashboard's
    /// WebSocket handler `register`s a fresh subscription, then sends
    /// `StartLogStream` ControllerMessages via `routing.register_sender` to
    /// each involved host. Inbound `AgentMessage::LogChunk` frames are
    /// routed through this fanout to the matching WebSocket task.
    pub log_fanout: Arc<log_fanout::LogFanout>,
    /// v0.3d: pending `WriteCompose` request resolver. Dashboard
    /// registers a oneshot keyed by request_id; the gRPC handler
    /// resolves it when the matching `WriteComposeAck` lands.
    pub compose_broker: Arc<compose_broker::ComposeBroker>,
    /// v0.3.6: Isengard-managed encrypted secrets store. Operators put
    /// secrets via `isd secret put` (or the installer's bootstrap
    /// subcommand); agents fetch over mTLS at container start and mount
    /// as tmpfs at `/run/secrets/<name>`. The store is "unlocked" when
    /// the master key file (`ISENGARD_MASTER_KEY_FILE`, default
    /// `/run/secrets/master.key`) is readable and 32 bytes long;
    /// otherwise the controller refuses to start.
    pub secrets: Arc<secrets::SecretsStore>,
}

/// Journal an event then broadcast it on the bus. Used by both the Sync
/// handler (for agent-originated events) and controller-internal producers
/// like `disconnect_monitor`.
///
/// On journal write failure, broadcasts NO event: better to drop than to
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
    /// v0.3b: zone name served by the embedded DNS resolver (e.g. `iso`).
    /// Empty string = resolver disabled, zero overhead.
    pub dns_zone: String,
    /// v0.3b: address the embedded DNS resolver binds to (UDP). Defaults to
    /// `0.0.0.0:5300` for dev; production compose maps :53 with
    /// `cap_add: NET_BIND_SERVICE`. Ignored when `dns_zone` is empty.
    pub dns_listen: SocketAddr,
    /// DNS-01 ACME wildcard cert config. Empty = subsystem disabled.
    pub acme: acme::AcmeConfig,
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

    // v0.3.6: managed secrets store. Reads the raw 32-byte master key
    // from the file at `ISENGARD_MASTER_KEY_FILE` (default
    // `/run/secrets/master.key`, bind-mounted by the install compose
    // recipe). Initialised early so the ACME bootstrap can fall back to
    // the encrypted store for the CF token when the env var is absent.
    let secrets_store = Arc::new(
        secrets::SecretsStore::from_env(inventory.clone())
            .context("loading master key for secrets store (set ISENGARD_MASTER_KEY_FILE or write /run/secrets/master.key)")?,
    );
    secrets_store
        .boot_check()
        .await
        .context("secrets store boot check")?;
    info!("secrets store unlocked (master key loaded)");

    // ACME (DNS-01) subsystem. Optional: only initialised when the operator
    // has configured email + token + at least one domain. The wildcard cert
    // store is plumbed into the routing pusher so every push includes the
    // current cert material.
    //
    // CF token resolution: env-provided ISENGARD_CF_DNS_API_TOKEN wins; if
    // absent, fall back to the encrypted secrets store under the name
    // `cf_dns_api_token` (set by the installer). This means the install
    // recipe can keep the token entirely out of env / compose config.
    let mut acme_with_token = opts.acme.clone();
    if acme_with_token.cf_api_token.is_none() {
        match secrets_store.fetch("cf_dns_api_token").await {
            Ok(value) => match String::from_utf8(value) {
                Ok(tok) => {
                    info!("acme: cf_dns_api_token loaded from encrypted secrets store");
                    acme_with_token.cf_api_token = Some(tok);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "acme: cf_dns_api_token in secrets store is not valid utf-8; ignoring");
                }
            },
            Err(secrets::SecretsError::NotFound(_)) => {
                tracing::debug!("acme: no cf_dns_api_token in secrets store");
            }
            Err(e) => {
                tracing::warn!(error = %e, "acme: failed to read cf_dns_api_token from secrets store");
            }
        }
    }
    let validated_acme = acme_with_token.validated();

    // Wildcard cert store always exists, regardless of ACME config. Certs
    // can come from ACME (via the scheduler), manual upload (future), or
    // be hydrated from SQLite at boot (migration 0026). The agent-facing
    // push side reads from this store independent of how the certs got
    // there. A controller restart loses the HashMap; the hydrate below
    // refills it from persistent storage so the agent receives the same
    // cert material via the next ProxyConfig push.
    let wildcard_store = Arc::new(acme::WildcardCertStore::new());
    match wildcard_store.hydrate_from(&inventory).await {
        Ok(0) => {
            info!("tls: no persisted wildcard certs to hydrate");
        }
        Ok(n) => {
            info!(count = n, "tls: hydrated wildcard certs from storage");
        }
        Err(e) => {
            warn!("tls: hydrate from storage failed; starting with empty cache: {e:#}");
        }
    }

    let routing = Arc::new(
        routing::RoutingPusher::new(inventory.clone()).with_wildcard_certs(wildcard_store.clone()),
    );
    let policy_ingest = Arc::new(policy_ingest::PolicyLabelIngest::new(inventory.clone()));

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
    let log_fanout = log_fanout::LogFanout::new();
    let compose_broker = Arc::new(compose_broker::ComposeBroker::new());

    let handles = Arc::new(ControllerHandles {
        inventory: inventory.clone(),
        journal: journal.clone(),
        bus: bus.clone(),
        routing: routing.clone(),
        enrollment: enrollment.clone(),
        revocation: revocation.clone(),
        db_path: db_path.clone(),
        log_fanout: log_fanout.clone(),
        compose_broker: compose_broker.clone(),
        secrets: secrets_store.clone(),
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

    // v0.3b: optional DNS resolver. Off by default (`--dns-zone ""`); when a
    // zone is configured the resolver binds to `dns_listen` and serves A
    // queries derived from `routing_rules`. Only active for the configured
    // zone; out-of-zone queries are REFUSED so the OS falls through to its
    // upstream resolver. Coexists additively with the agent's mDNS responder
    // (v0.3a) which handles `.local`.
    let dns_handle = if opts.dns_zone.trim().is_empty() {
        None
    } else {
        let resolver = dns::DnsResolver::new(opts.dns_zone.clone(), inventory.clone());
        match resolver.start(opts.dns_listen).await {
            Ok(handle) => {
                info!(
                    zone = %opts.dns_zone,
                    listen = %opts.dns_listen,
                    "DNS resolver started",
                );
                Some(handle)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    zone = %opts.dns_zone,
                    listen = %opts.dns_listen,
                    "DNS resolver failed to start; controller continues without it",
                );
                None
            }
        }
    };

    // ACME (DNS-01) wildcard cert scheduler. Spawns only when the validated
    // config has email + token + domains. Misconfigured deployments fail
    // closed: token verification at boot prints an explicit warning so the
    // operator notices before the scheduler tries (and rate-limits) LE.
    if let Some(cfg) = validated_acme.clone() {
        let token = cfg.cf_api_token.clone();
        let email = cfg.email.clone();
        let directory = cfg.directory_url.clone();
        let groups = cfg.groups.clone();
        let inv_for_acme = inventory.clone();
        let store_for_routing = wildcard_store.clone();
        // Verify the CF token at boot in a separate task so a slow CF API
        // doesn't delay gRPC bind. The scheduler still spawns; if the token
        // is bad it'll fail in `present` with a clear error.
        let token_for_verify = token.clone();
        tokio::spawn(async move {
            let api = acme::CloudflareApi::new(token_for_verify);
            match api.verify_token().await {
                Ok(()) => info!("acme: Cloudflare API token verified"),
                Err(e) => tracing::warn!(error = %e, "acme: Cloudflare API token verify failed"),
            }
        });

        let dns_provider = acme::CloudflareDnsProvider::new(token);
        let acme_client = Arc::new(acme::AcmeDns01Client::new(
            inv_for_acme,
            email,
            directory.clone(),
            dns_provider,
        ));
        info!(
            directory = %directory,
            groups = groups.len(),
            "acme: DNS-01 scheduler enabled",
        );
        // Hand the routing pusher to the scheduler so a freshly-issued or
        // renewed wildcard cert kicks an immediate fan-out to every
        // connected agent. The 5-minute sweeper this replaced was a
        // placeholder; the right model is event-driven. Routing-rule edits
        // (dashboard create/update/delete) also push synchronously from
        // their handlers (see crates/isengard-plugins/dashboard/src/routing.rs).
        acme::spawn_renewal_scheduler(
            inventory.clone(),
            store_for_routing.clone(),
            acme_client.clone(),
            groups,
            Some(routing.clone()),
        );
    }

    // Phase 10c (10i, refs #50): stack-level deployment orchestrator. Owns the
    // multi-host wave plan when a stack-wide update fans out to 2+ hosts.
    // Single-host deploys bypass this entirely; existing per-host deployment
    // supervisors keep their behaviour.
    let orchestrator =
        std::sync::Arc::new(stack_deploy_orchestrator::StackDeployOrchestrator::new(
            inventory.clone(),
            bus.clone(),
            std::sync::Arc::new(stack_deploy_orchestrator::ProductionDispatcher::new(
                inventory.clone(),
            )),
        ));
    let orchestrator_handle = orchestrator.clone().start_background();

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
    // Phase 0.10: extra SANs threaded through `isengard init` so the
    // controller's server cert is valid for `localhost`, `127.0.0.1`,
    // the auto-detected host IP, and any operator-supplied hostnames.
    // Comma-separated; whitespace and blank entries are ignored.
    let extra_sans: Vec<String> = std::env::var("ISENGARD_EXTRA_SANS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // Imp-1: controller's own server cert needs both ClientAuth and
    // ServerAuth. Agents only get ClientAuth via sign_agent_leaf.
    let server_leaf = ca
        .sign_server_leaf_with_sans(
            HostId::new(),
            &controller_dns,
            &extra_sans,
            chrono::Duration::days(30),
        )
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
        inventory.clone(),
        journal,
        bus,
        routing.clone(),
        policy_ingest.clone(),
        ca,
        enrollment,
        revocation,
        log_fanout,
        compose_broker.clone(),
        secrets_store.clone(),
    ));

    // Phase 9b.1: periodic reaper for orphaned container-scope policy rows.
    // Runs every hour with a 24h max-age. Belt-and-braces against missed
    // `ContainerLabelsRemoved` events (e.g. agent crashed during destroy).
    let reaper_inv = inventory.clone();
    let reaper_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        // Skip the first immediate tick: nothing useful to do at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = chrono::Utc::now();
            match policy_ingest::reap_orphaned_container_policies(
                &reaper_inv,
                now,
                chrono::Duration::hours(24),
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(reaped = n, "policy labels: reaper swept stale rows"),
                Err(e) => {
                    tracing::warn!(error = %e, "policy labels: reaper failed");
                }
            }
        }
    });

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
    reaper_handle.abort();
    orchestrator_handle.abort();
    if let Some(h) = dns_handle {
        h.abort();
    }

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
