#![doc = include_str!("../docs/_crate.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod service;

pub mod acme;
pub mod auth;
pub mod bus;
pub mod ca;
pub mod cloudflare;
pub mod compose_broker;
pub mod config;
pub mod container_id;
pub mod disconnect_monitor;
pub mod dns;
pub mod enrollment;
pub mod hook_ingest;
pub mod log_fanout;
pub mod pending_actions;
pub mod plugin_host;
pub mod policy_ingest;
pub mod revocation;
pub mod routing;
pub mod scheduler;
pub mod secrets;
pub mod ssh_ca;
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
use crate::ssh_ca::SshAuthority;

/// Bundle of long-lived references controller-side plugins need.
///
/// Built by [`run_controller`] after every primitive has booted, then
/// downcast through `Arc<dyn Any>` on the [`isengard_core::PluginContext`]
/// so plugins can reach into the controller's state without depending on
/// this crate's private types.
#[derive(Clone)]
pub struct ControllerHandles {
    /// Shared inventory pool. Every DAO call (hosts, stacks, services,
    /// routing rules, deployments, placements, policies) lands here.
    pub inventory: Arc<Inventory>,
    /// Append-only event log. Shares the SQLite file with
    /// `inventory`; the two facades co-migrate.
    pub journal: Arc<Journal>,
    /// Broadcast surface for `Event`. Plugins subscribe at start.
    pub bus: Arc<EventBus>,
    /// Agent-facing routing reconciler. Plugins push routing
    /// changes by mutating the inventory then calling
    /// [`routing::RoutingPusher::push_to_host`].
    pub routing: Arc<routing::RoutingPusher>,
    /// Enrollment-token mint and redeem.
    ///
    /// The dashboard plugin exposes the REST endpoints that wrap
    /// [`EnrollmentService::mint`] and present packed tokens to the
    /// operator.
    pub enrollment: Arc<EnrollmentService>,
    /// In-memory revocation set the auth interceptor reads on every RPC.
    ///
    /// The dashboard's revoke handler calls [`revocation::revoke_agent`]
    /// which writes the DB row and updates this set in one pass.
    pub revocation: RevocationSet,
    /// On-disk path to the controller's SQLite file.
    ///
    /// The backup plugin opens its own pool against this path for a WAL
    /// checkpoint plus file copy. Surfaced rather than exposing
    /// `Inventory::pool()`.
    pub db_path: std::path::PathBuf,
    /// Log-streaming subscription registry.
    ///
    /// The dashboard WebSocket handler registers a subscription, dispatches
    /// `StartLogStream` `ControllerMessage`s to the involved hosts via the
    /// routing pusher's sender registry, and reads chunks back through this
    /// fanout as inbound `AgentMessage::LogChunk` frames land on the Sync
    /// stream.
    pub log_fanout: Arc<log_fanout::LogFanout>,
    /// Compose-write request resolver (v0.3d).
    ///
    /// The dashboard's `PUT /api/v1/stacks/<id>/compose` registers a
    /// oneshot keyed by `request_id`, sends `WriteCompose` to the host,
    /// and awaits the matching `WriteComposeAck` here.
    pub compose_broker: Arc<compose_broker::ComposeBroker>,
    /// Isengard-managed encrypted secrets store (v0.3.6).
    ///
    /// Operators write secrets via `isd secret set` or the installer's
    /// bootstrap path; agents fetch over mTLS at container start and
    /// mount as tmpfs at `/run/secrets/<name>`. The store unlocks when
    /// the master key file (`ISENGARD_MASTER_KEY_FILE`, default
    /// `/run/secrets/master.key`) is readable and 32 bytes long;
    /// otherwise [`run_controller`] refuses to start.
    pub secrets: Arc<secrets::SecretsStore>,
    /// Controller's internal CA.
    ///
    /// Same [`ca::Authority`] held by [`EnrollmentService`] (for leaf
    /// signing) and [`service::ControllerService`] (for the
    /// unauthenticated `GetCaPem` RPC). Plugins read the root cert here
    /// when they need to.
    pub ca: Arc<Authority>,
    /// SSH user-certificate authority. Sibling primitive to `ca`: the
    /// controller mints short-lived OpenSSH user certs through this
    /// surface, and every fleet host trusts the corresponding public
    /// key via a `TrustedUserCAKeys` directive installed by the agent
    /// at enroll time. The cert-issuance RPC and operator CLI land in
    /// later phases; this handle is the shared signing primitive.
    pub ssh_ca: Arc<SshAuthority>,
    /// Schema-driven dispatcher for `isd configure`.
    ///
    /// Reads and writes every operator-settable key, routing secret
    /// keys to `secrets` and non-secret keys to a [`SettingStore`]
    /// wrapping the same inventory pool. Built at boot off
    /// [`config::Schema::v01`]; the dashboard's configure routes
    /// (added in PR 2 of this track) hang off this handle.
    pub config_dispatcher: Arc<config::ConfigDispatcher>,
}

impl ControllerHandles {
    /// Borrow the schema-driven `isd configure` dispatcher.
    pub fn config_dispatcher(&self) -> &Arc<config::ConfigDispatcher> {
        &self.config_dispatcher
    }

    /// Build a [`config::ConfigDispatcher`] backed by `inventory` and
    /// `secrets`. Convenience for tests that construct
    /// [`ControllerHandles`] directly without going through
    /// [`run_controller`]; production code calls
    /// [`config::ConfigDispatcher::new`] inline.
    pub fn test_config_dispatcher(
        inventory: Arc<Inventory>,
        secrets: Arc<secrets::SecretsStore>,
    ) -> Arc<config::ConfigDispatcher> {
        let setting_store = Arc::new(isengard_storage::SettingStore::new(inventory));
        Arc::new(config::ConfigDispatcher::new(
            config::Schema::v01(),
            secrets,
            setting_store,
        ))
    }
}

/// Journals an event then publishes it to the bus.
///
/// Used by the Sync handler for agent-originated events and by
/// controller-internal producers ([`disconnect_monitor`], the scheduler,
/// the enrollment success path). The journal write happens first; a
/// failure there returns without publishing so a downstream notifier
/// never reacts to an event we have no record of.
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

/// Boot configuration for [`run_controller`].
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// Address the gRPC server binds to (e.g. `0.0.0.0:9417`).
    pub listen: SocketAddr,
    /// Directory where mutable state lives. The SQLite database and the
    /// future config cache land here. Created if missing.
    pub state_dir: std::path::PathBuf,
    /// Per-plugin config slices keyed by plugin name.
    pub config: serde_json::Value,
    /// Zone served by the embedded DNS resolver (e.g. `iso`).
    ///
    /// Empty string disables the resolver entirely; the boot path skips
    /// the socket bind and the polling task.
    pub dns_zone: String,
    /// Address the embedded DNS resolver binds to (UDP).
    ///
    /// Defaults to `0.0.0.0:5300` for dev; the production compose recipe
    /// maps `:53` with `cap_add: NET_BIND_SERVICE`. Ignored when
    /// `dns_zone` is empty.
    pub dns_listen: SocketAddr,
    /// DNS-01 ACME wildcard cert config. Empty disables the subsystem.
    pub acme: acme::AcmeConfig,
}

/// Discovers and instantiates every plugin that advertises
/// `Capability::Controller`.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Controller)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Runs controller mode end to end.
///
/// Boots inventory and journal, unlocks the secrets store, loads or
/// generates the CA, hydrates the revocation set, starts plugins, spawns
/// the placement scheduler, disconnect monitor, deployment orchestrator,
/// DNS resolver (if configured), and ACME scheduler (if configured),
/// then binds the gRPC server. Returns when SIGINT lands and the server
/// finishes draining.
///
/// # Errors
///
/// Returns `Err` when any boot step fails: opening the SQLite file,
/// reading the master key, signing the controller's own server cert,
/// binding the listener, or installing the server TLS config.
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

    // `isd configure` dispatcher. Wraps the inventory's `settings`
    // table behind a SettingStore and pairs it with the existing
    // SecretsStore; the static schema routes per-key writes to the
    // right backing store. Built here (rather than later) so the
    // routing pusher can read `acme.contact_email` from it on every
    // `build_for_host`.
    let setting_store = Arc::new(isengard_storage::SettingStore::new(inventory.clone()));
    let config_dispatcher = Arc::new(config::ConfigDispatcher::new(
        config::Schema::v01(),
        secrets_store.clone(),
        setting_store,
    ));

    let routing = Arc::new(
        routing::RoutingPusher::new(inventory.clone())
            .with_wildcard_certs(wildcard_store.clone())
            .with_config_dispatcher(config_dispatcher.clone()),
    );
    let policy_ingest = Arc::new(policy_ingest::PolicyLabelIngest::new(inventory.clone()));
    // Lifecycle-hook label ingest runs in parallel with policy_ingest.
    let hook_ingest = Arc::new(hook_ingest::HookLabelIngest::new(inventory.clone()));

    // Internal CA + enrollment service. CA is loaded-or-initialized
    // from the `ca` row (single-row table); the EnrollmentService owns the
    // mint/redeem flow and signs leaf certs via the CA.
    let ca = Arc::new(
        Authority::load_or_init(&inventory)
            .await
            .context("loading or initializing CA")?,
    );
    let enrollment = Arc::new(EnrollmentService::new(inventory.clone(), ca.clone()));

    // SSH user-certificate authority. Sibling primitive to the TLS CA.
    // Loaded-or-initialized from the encrypted secrets store under the
    // row name `ssh_ca_private_key`. Same envelope as `master.key`: a
    // fresh install mints an ed25519 keypair, subsequent boots reuse it.
    let ssh_ca = Arc::new(
        SshAuthority::load_or_init(&secrets_store)
            .await
            .context("loading or initializing SSH CA")?,
    );
    info!("ssh ca loaded");

    // Hydrate the in-memory revocation set from the `agent_certs`
    // table so the very first RPC after boot already sees revoked certs.
    let revocation = RevocationSet::load_from_inventory(&inventory)
        .await
        .context("hydrating revocation set")?;

    // -- plugin init/start ----------------------------------------------------
    // Load + start controller-side plugins (notifier, etc).
    let log_fanout = log_fanout::LogFanout::new();
    let compose_broker = Arc::new(compose_broker::ComposeBroker::new());

    // `config_dispatcher` is built earlier (before the routing pusher)
    // so the pusher can read `acme.contact_email` from it. The dashboard
    // routes added in PR 2 of the configure track hang off the same
    // handle that lives on `ControllerHandles`.

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
        ca: ca.clone(),
        ssh_ca: ssh_ca.clone(),
        config_dispatcher,
    });
    let mut controller_plugins =
        plugin_host::load_controller_plugins(handles, opts.config.clone()).await;
    info!(
        plugin_count = controller_plugins.len(),
        "controller plugins started"
    );

    // Placement scheduler. Constructed BEFORE the
    // disconnect monitor so the monitor can be wired to call
    // on_host_disconnect_long whenever it fires. The scheduler
    // itself rebuilds in-memory state from the placements +
    // agent_labels tables (the migration backfilled pre-0.14
    // services).
    //
    // PlacementSource lookups land in step 7; the skeleton uses
    // EmptyPlacementSource so reconcile_all is a no-op until then.
    let placement_scheduler = std::sync::Arc::new(
        scheduler::Scheduler::new(
            inventory.clone(),
            bus.clone(),
            std::time::Duration::from_secs(scheduler::DEFAULT_GRACE_SECS),
            std::sync::Arc::new(scheduler::EmptyPlacementSource),
        )
        .await
        .context("initialising placement scheduler")?,
    );
    let scheduler_handle = placement_scheduler.clone().start();

    // Background task: detect long-disconnected agents and emit
    // `agent.disconnect_long` (4h threshold, 60s poll cadence in production).
    let disconnect_monitor = std::sync::Arc::new(
        disconnect_monitor::DisconnectMonitor::new(
            inventory.clone(),
            journal.clone(),
            bus.clone(),
            14400, // 4 hours
            60.0,  // 60s poll
        )
        .with_scheduler(placement_scheduler.clone()),
    );
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

    // Stack-level deployment orchestrator. Owns the
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

    // Per-RPC client cert validation + revocation check (replaces
    // the bearer-token middleware).
    let auth_layer = CertAuthInterceptor::new(revocation.clone(), ca.clone());

    // Sign the controller's own server cert with our CA. The DNS name agents
    // verify against is configurable so a single binary can be deployed under
    // different hostnames; the default matches the test fixture so dev
    // workflows don't require extra env wiring.
    let controller_dns =
        std::env::var("ISENGARD_CONTROLLER_DNS").unwrap_or_else(|_| "controller.local".into());
    // Extra SANs threaded through `isengard init` so the
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
        hook_ingest.clone(),
        ca,
        enrollment,
        revocation,
        log_fanout,
        compose_broker.clone(),
        secrets_store.clone(),
        Some(placement_scheduler.clone()),
        ssh_ca.clone(),
    ));

    // Periodic reaper for orphaned container-scope policy rows.
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
    scheduler_handle.abort();
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
