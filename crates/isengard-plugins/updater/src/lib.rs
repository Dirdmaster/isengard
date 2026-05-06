//! Isengard `updater` plugin.
//!
//! Watches running Docker containers and (in later sub-phases) keeps their
//! images up to date. Phase 3b: filters containers by `isengard.enable=true`,
//! compares each one's local digest against its remote registry digest, and
//! classifies as `up_to_date | needs_update | unknown`.

#![allow(clippy::result_large_err)]

pub mod auth;
pub mod dispatch_helpers;
pub mod image_ref;
pub mod labels;
pub mod policy;
pub mod recreate;
pub mod registry;
pub mod self_id;
pub mod self_update;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::ListContainersOptions;
use chrono::Utc;
use isengard_core::{
    AgentPlugin, Capability, CoreError, DispatchOutcome, Event, EventEmitter, HostId, LoadedPolicy,
    Plugin, PluginContext, PluginRegistration, PolicyLoader, Result, UpdateDispatcher,
    UpdateTriggerInfo,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;
use crate::labels::isengard_enabled;
use crate::registry::RegistryClient;

const PLUGIN_NAME: &str = "updater";
const DEFAULT_CYCLE_INTERVAL_SECS: u64 = 30;
const MIN_CYCLE_INTERVAL_SECS: u64 = 5;

pub struct Updater {
    /// Lazily set in `init`. Wrapped in Option so the struct can be constructed
    /// by the inventory factory before init runs.
    docker: Option<Docker>,
    registry: Option<Arc<RegistryClient>>,
    cycle_interval: Duration,
    emitter: Option<Arc<dyn EventEmitter>>,
    /// Set in `init` from `PluginContext::update_dispatcher`. When `Some`,
    /// the cycle consults it before recreating any non-self container —
    /// the dispatcher may take ownership and spawn a blue-green driver.
    dispatcher: Option<Arc<dyn UpdateDispatcher>>,
    /// Set in `init` from `PluginContext::host_id`. Forwarded into every
    /// `UpdateTriggerInfo` so the dispatcher's downstream lookups
    /// (routing rules, deployment dedupe) target the right host.
    host_id: Option<HostId>,
    /// Set in `init` from `PluginContext::policy_loader`. When `Some`, the
    /// cycle pulls the full policy snapshot at the start and resolves
    /// per-candidate (Phase 9b: respects Pinned + paused_until).
    policy_loader: Option<Arc<dyn PolicyLoader>>,
    /// Cached fleet name for this host. Looked up once during `init` (when
    /// both a policy_loader and a host_id are wired) so the per-cycle path
    /// has zero extra DB hits. `None` means "no fleet-scoped rows match".
    fleet: Option<String>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
            cycle_interval: Duration::from_secs(DEFAULT_CYCLE_INTERVAL_SECS),
            emitter: None,
            dispatcher: None,
            host_id: None,
            policy_loader: None,
            fleet: None,
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

// --- error-wrapping helpers --------------------------------------------------
// The Plugin trait returns `isengard_core::Result<()>` which is
// `Result<(), CoreError>`. Bollard / anyhow errors don't auto-coerce, so we
// wrap them per-lifecycle-stage. Phase 3b can refactor if a `From` impl lands
// in isengard-core.

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

async fn emit(emitter: Option<&Arc<dyn EventEmitter>>, event: Event) {
    if let Some(e) = emitter {
        e.emit(event).await;
    }
}

/// One cycle of work. Filter candidates by `isengard.enable=true`, compare
/// each one's local digest against its remote registry digest, classify, log.
///
/// `dispatcher` is consulted on the non-self `needs_update` path: if it
/// returns `Handled`, the in-place recreate is skipped (a blue-green driver
/// has taken over). If it returns `PerformInPlace`, we fall through to the
/// existing `recreate::update_container` call. `host_id` is forwarded into
/// every `UpdateTriggerInfo`.
///
/// Phase 9b: when `policy_loader` is `Some`, the cycle pulls the policy
/// snapshot once at the start, resolves a `ResolvedPolicy` per candidate
/// (using the candidate's compose labels + cached fleet + host_id), and
/// short-circuits with `update.policy_skipped` for `Pinned` services and
/// services with active `paused_until`. All other resolved-policy fields
/// are computed but NOT enforced; Phase 9e+ adds them.
#[allow(clippy::too_many_arguments)]
async fn do_cycle(
    docker: &Docker,
    registry: &RegistryClient,
    emitter: Option<&Arc<dyn EventEmitter>>,
    dispatcher: Option<&Arc<dyn UpdateDispatcher>>,
    host_id: Option<HostId>,
    policy_loader: Option<&Arc<dyn PolicyLoader>>,
    fleet: Option<&str>,
) -> anyhow::Result<()> {
    let opts = ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    };
    let containers = docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("listing containers: {e}"))?;

    let candidates: Vec<_> = containers
        .iter()
        .filter(|c| isengard_enabled(c.labels.as_ref()))
        .collect();

    // Phase 9b: load the policy snapshot once per cycle. On loader error
    // we behave as if no policies exist (fail-safe: don't block updates).
    let policy_snapshot: Vec<LoadedPolicy> = match policy_loader {
        Some(loader) => match loader.list().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "policy loader list failed; running cycle without policies");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let host_id_hex = host_id.map(|h| h.to_string());

    let mut up_to_date = 0usize;
    let mut needs_update = 0usize;
    let mut unknown = 0usize;
    let mut pinned = 0usize;
    let mut paused = 0usize;

    for c in &candidates {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let image_str = c.image.as_deref().unwrap_or("");

        let Some(image_ref) = ImageRef::parse(image_str) else {
            debug!(container = %name, image = %image_str, "skipping digest-pinned or unparseable image");
            continue;
        };

        // Phase 9b policy gate: build the resolver context from the
        // candidate's compose labels + cached fleet + host_id, then
        // short-circuit on Pinned / paused_until.
        let owned_ctx = crate::policy::policy_context_from_container(
            c.labels.as_ref(),
            fleet,
            host_id_hex.as_deref(),
            &name,
        );
        let projected = crate::policy::project_for_resolver(&policy_snapshot);
        let resolved = isengard_core::policy::resolve_policy(&projected, &owned_ctx.as_ref());
        match crate::policy::decision_from_resolved(&resolved, Utc::now()) {
            crate::policy::PolicyDecision::Skip(reason) => {
                let event =
                    build_policy_skipped_event(&owned_ctx, &name, host_id_hex.as_deref(), &reason);
                match reason {
                    crate::policy::SkipReason::Pinned => {
                        info!(container = %name, reason = "pinned", "policy skip");
                        pinned += 1;
                    }
                    crate::policy::SkipReason::Paused { until } => {
                        info!(container = %name, reason = "paused", until = %until, "policy skip");
                        paused += 1;
                    }
                }
                emit(emitter, event).await;
                continue;
            }
            crate::policy::PolicyDecision::Proceed => {}
        }

        let local_digest = match docker.inspect_image(image_str).await {
            Ok(i) => i
                .repo_digests
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|d| d.split_once('@'))
                .map(|(_, dig)| dig.to_string()),
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "inspect_image failed");
                None
            }
        };

        let remote_digest = match registry.head_digest(&image_ref).await {
            Ok(d) => d,
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "registry HEAD failed");
                unknown += 1;
                continue;
            }
        };

        match (local_digest.as_deref(), remote_digest.as_deref()) {
            (Some(local), Some(remote)) if local == remote => {
                info!(container = %name, image = %image_str, status = "up_to_date");
                up_to_date += 1;
            }
            (Some(local), Some(remote)) => {
                info!(
                    container = %name,
                    image = %image_str,
                    current_digest = %local,
                    remote_digest = %remote,
                    status = "needs_update"
                );
                needs_update += 1;

                // Self-update: when the candidate is this process's own
                // container, use rename-then-replace ordering (3d).
                if let Some(self_id) = crate::self_id::current_container_id() {
                    if c.id.as_deref() == Some(self_id.as_str()) {
                        info!(container = %name, "self-update path: rename-then-replace");
                        match self_update::update_self(docker, &self_id, &image_ref, emitter).await
                        {
                            Ok(()) => {
                                // update_self schedules process exit; bail out
                                // of the cycle so we don't keep working.
                                info!("self-update succeeded; cycle ending early");
                                return Ok(());
                            }
                            Err(e) => {
                                warn!(error = %e, "self-update failed; will retry next cycle");
                                continue;
                            }
                        }
                    }
                }

                let Some(container_id) = c.id.as_deref() else {
                    warn!(container = %name, "no container ID; cannot update");
                    continue;
                };

                // Dispatcher hand-off: if the agent has installed an
                // UpdateDispatcher (i.e. blue-green is wired up), inspect
                // the container, build an UpdateTriggerInfo, and let the
                // dispatcher decide. If it returns Handled, a driver has
                // taken over and we must NOT recreate.
                if let Some(disp) = dispatcher {
                    match docker.inspect_container(container_id, None).await {
                        Ok(inspect) => {
                            let info = UpdateTriggerInfo {
                                container_id: container_id.to_string(),
                                service_name: dispatch_helpers::service_name(c, &inspect),
                                stack_id: 0,
                                host_id: host_id.unwrap_or_else(HostId::nil),
                                blue_digest: local.to_string(),
                                green_digest: remote.to_string(),
                                image_ref: image_str.to_string(),
                                container_port: dispatch_helpers::first_container_port(&inspect),
                                has_healthcheck: dispatch_helpers::has_healthcheck(&inspect),
                                rw_volume_mounts: dispatch_helpers::rw_volume_mounts(&inspect),
                                label_strategy: dispatch_helpers::label_strategy(&inspect),
                            };
                            match disp.dispatch(info).await {
                                DispatchOutcome::Handled => {
                                    debug!(container = %name, "dispatcher handled — skipping in-place recreate");
                                    continue;
                                }
                                DispatchOutcome::PerformInPlace => {
                                    debug!(container = %name, "dispatcher said in-place — falling through to recreate");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(container = %name, error = %e, "inspect for dispatcher failed; falling through to in-place recreate");
                        }
                    }
                }

                match recreate::update_container(docker, container_id, &image_ref).await {
                    Ok(()) => {
                        emit(
                            emitter,
                            Event {
                                kind: isengard_core::event::kinds::UPDATE_SUCCESS.into(),
                                occurred_at: Utc::now(),
                                summary: format!("updated {name} to {remote}"),
                                container_name: Some(name.clone()),
                                image: Some(image_str.to_string()),
                                old_digest: Some(local.to_string()),
                                new_digest: Some(remote.to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        warn!(container = %name, error = %err_str, "update failed");
                        emit(
                            emitter,
                            Event {
                                kind: isengard_core::event::kinds::UPDATE_FAILED.into(),
                                occurred_at: Utc::now(),
                                summary: format!("update failed for {name}: {err_str}"),
                                container_name: Some(name.clone()),
                                image: Some(image_str.to_string()),
                                error: Some(err_str),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                }
            }
            _ => {
                debug!(container = %name, image = %image_str, "could not classify (missing local or remote digest)");
                unknown += 1;
            }
        }
    }

    info!(
        candidates = candidates.len(),
        up_to_date, needs_update, unknown, pinned, paused, "updater cycle complete"
    );

    emit(
        emitter,
        Event {
            kind: isengard_core::event::kinds::UPDATE_CHECKED.into(),
            occurred_at: Utc::now(),
            summary: format!(
                "cycle: candidates={} up_to_date={} needs_update={} unknown={} pinned={} paused={}",
                candidates.len(),
                up_to_date,
                needs_update,
                unknown,
                pinned,
                paused,
            ),
            metadata: serde_json::json!({
                "candidates": candidates.len(),
                "up_to_date": up_to_date,
                "needs_update": needs_update,
                "unknown": unknown,
                "pinned": pinned,
                "paused": paused,
            }),
            ..Default::default()
        },
    )
    .await;
    Ok(())
}

/// Construct the `update.policy_skipped` event payload defined by the
/// Phase 9b spec:
///
/// ```json
/// {
///   "service": "...",
///   "container": "...",
///   "host_id": "...",
///   "reason": "pinned" | "paused",
///   "until": "<RFC3339, paused only>"
/// }
/// ```
///
/// The `service` field falls back to the container name when the candidate
/// isn't a compose-managed container (so downstream notifier rules always
/// have a non-empty value to display).
fn build_policy_skipped_event(
    ctx: &crate::policy::OwnedPolicyContext,
    container_name: &str,
    host_id_hex: Option<&str>,
    reason: &crate::policy::SkipReason,
) -> Event {
    let service = ctx
        .service
        .clone()
        .unwrap_or_else(|| container_name.to_string());
    let mut payload = serde_json::json!({
        "service": service,
        "container": container_name,
        "host_id": host_id_hex.unwrap_or(""),
        "reason": reason.as_str(),
    });
    if let crate::policy::SkipReason::Paused { until } = reason {
        payload["until"] = serde_json::Value::String(until.to_rfc3339());
    }
    let summary = match reason {
        crate::policy::SkipReason::Pinned => format!("skipped {service} (pinned)"),
        crate::policy::SkipReason::Paused { until } => {
            format!("skipped {service} (paused until {})", until.to_rfc3339())
        }
    };
    Event {
        kind: isengard_core::event::kinds::UPDATE_POLICY_SKIPPED.into(),
        occurred_at: Utc::now(),
        summary,
        container_name: Some(container_name.to_string()),
        metadata: payload,
        ..Default::default()
    }
}

#[async_trait]
impl Plugin for Updater {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cycle_interval_secs = ctx
            .config
            .get("cycle_interval_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_CYCLE_INTERVAL_SECS)
            .max(MIN_CYCLE_INTERVAL_SECS);
        self.cycle_interval = Duration::from_secs(cycle_interval_secs);
        info!(cycle_interval_secs, "updater cycle interval configured");

        // Connect to the local Docker daemon. Honors DOCKER_HOST + standard
        // socket paths.
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| init_err(format!("connecting to docker daemon: {e}")))?;

        // Verify connectivity early — a bad daemon URL or missing socket
        // should fail init, not silently break the cycle.
        let version = docker
            .version()
            .await
            .map_err(|e| init_err(format!("docker version probe failed: {e}")))?;
        info!(
            api_version = %version.api_version.as_deref().unwrap_or("?"),
            engine_version = %version.version.as_deref().unwrap_or("?"),
            "updater connected to docker daemon"
        );

        let docker_config = DockerConfig::load_default().unwrap_or_else(|e| {
            warn!(error = %e, "failed to read ~/.docker/config.json — proceeding without registry creds");
            DockerConfig::default()
        });
        let registry = RegistryClient::new(docker_config)
            .map_err(|e| init_err(format!("registry client: {e}")))?;
        self.registry = Some(Arc::new(registry));

        // If we're running inside a container, clean up any leftover
        // `<our-name>-replaced-*` siblings from a prior self-update.
        if let Some(my_id) = crate::self_id::current_container_id() {
            match docker.inspect_container(&my_id, None).await {
                Ok(self_inspect) => {
                    let my_name = self_inspect
                        .name
                        .as_deref()
                        .map(|s| s.trim_start_matches('/').to_string())
                        .unwrap_or_default();
                    if !my_name.is_empty() {
                        crate::self_update::cleanup_replaced_siblings(&docker, &my_name).await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "could not inspect self container; skipping replaced-sibling cleanup");
                }
            }
        }

        self.docker = Some(docker);

        // Pick up the agent's EventEmitter (None if running on controller side
        // or in a test that didn't wire one).
        self.emitter = ctx.events.clone();
        if self.emitter.is_some() {
            info!("updater wired to event emitter");
        }
        // Optional blue-green hand-off (Phase 10). When present, the cycle
        // consults the dispatcher before recreating any container.
        self.dispatcher = ctx.update_dispatcher.clone();
        if self.dispatcher.is_some() {
            info!("updater wired to update dispatcher (blue-green path enabled)");
        }
        self.host_id = ctx.host_id;

        // Phase 9b: pick up the policy loader. When wired, the cycle
        // resolves a `ResolvedPolicy` per candidate and respects Pinned +
        // paused_until. Cache the host's fleet name once so per-cycle
        // resolution doesn't re-hit the DB for it.
        self.policy_loader = ctx.policy_loader.clone();
        if let (Some(loader), Some(host_id)) = (self.policy_loader.as_ref(), self.host_id) {
            match loader.fleet_for(host_id).await {
                Ok(fleet) => {
                    info!(
                        fleet = fleet.as_deref().unwrap_or("<none>"),
                        "updater wired to policy loader"
                    );
                    self.fleet = fleet;
                }
                Err(e) => {
                    warn!(error = %e, "policy loader fleet_for lookup failed; defaulting to None");
                    self.fleet = None;
                }
            }
        } else if self.policy_loader.is_some() {
            info!("updater wired to policy loader (no host_id; fleet=None)");
        }
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let registry = self
            .registry
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let emitter = self.emitter.clone();
        let dispatcher = self.dispatcher.clone();
        let host_id = self.host_id;
        let policy_loader = self.policy_loader.clone();
        let fleet = self.fleet.clone();
        let cancel = self.cancel.clone();
        let interval = self.cycle_interval;

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(
                            &docker,
                            &registry,
                            emitter.as_ref(),
                            dispatcher.as_ref(),
                            host_id,
                            policy_loader.as_ref(),
                            fleet.as_deref(),
                        ).await {
                            // Don't crash the task on a single bad cycle; just log
                            // and try again next tick. Phase 3b adds retry policy.
                            warn!(error = %e, "updater cycle failed");
                        }
                    }
                }
            }
        });

        self.task = Some(task);
        info!("updater started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.cancel.notify_waiters();
        if let Some(task) = self.task.take() {
            // Give the task a moment to exit cleanly; if it hangs, abort.
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Ok(())) => debug!("updater cycle task ended cleanly"),
                Ok(Err(e)) => warn!(error = %e, "updater cycle task panicked"),
                Err(_) => warn!("updater cycle task timed out on stop"),
            }
        }
        info!("updater stopped");
        Ok(())
    }
}

#[async_trait]
impl AgentPlugin for Updater {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        // External-invocation entry point. Phase 3a: same as the internal task.
        // Phase 3e+: controller-triggered "force update now" lands here.
        let docker = self
            .docker
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        do_cycle(
            docker,
            registry,
            self.emitter.as_ref(),
            self.dispatcher.as_ref(),
            self.host_id,
            self.policy_loader.as_ref(),
            self.fleet.as_deref(),
        )
        .await
        .map_err(|e| init_err(format!("cycle failed: {e}")))
    }
}

inventory::submit! {
    PluginRegistration {
        name: "updater",
        capabilities: &[Capability::Agent],
        constructor: || Box::new(Updater::new()) as Box<dyn Plugin>,
    }
}

// Compile-time assertion: Updater must remain Send + Sync because the
// inventory factory hands it across threads.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync + 'static>() {}
    assert::<Updater>();
}

#[cfg(test)]
mod emit_tests {
    use super::*;
    use std::sync::Mutex;

    /// Captures emitted events for assertion.
    struct RecordingEmitter {
        events: Mutex<Vec<Event>>,
    }

    impl RecordingEmitter {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn snapshot(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventEmitter for RecordingEmitter {
        async fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn emit_helper_skips_when_emitter_none() {
        // Should not panic — no emitter, no-op.
        emit(None, Event::default()).await;
    }

    #[tokio::test]
    async fn emit_helper_delivers_when_emitter_some() {
        let recorder = Arc::new(RecordingEmitter::new());
        let as_emitter: Arc<dyn EventEmitter> = recorder.clone();
        emit(
            Some(&as_emitter),
            Event {
                kind: "test.kind".into(),
                summary: "hello".into(),
                ..Default::default()
            },
        )
        .await;
        let snap = recorder.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].kind, "test.kind");
        assert_eq!(snap[0].summary, "hello");
    }
}
