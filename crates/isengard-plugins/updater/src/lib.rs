#![doc = include_str!("../docs/_crate.md")]
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]
#![allow(clippy::result_large_err)]

pub mod auth;
pub mod dispatch_helpers;
pub mod gate;
pub mod image_ref;
pub mod labels;
pub mod policy;
pub mod recreate;
pub mod registry;
pub mod self_id;
pub mod self_update;
pub mod tag_cache;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::ListContainersOptions;
use chrono::Utc;
use isengard_core::{
    AgentPlugin, ApprovalStore, Capability, CoreError, DispatchOutcome, Event, EventEmitter,
    HostId, InsertPendingApproval, LoadedPolicy, Plugin, PluginContext, PluginRegistration,
    PolicyLoader, Result, UpdateDispatcher, UpdateTriggerInfo,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::DockerConfig;
use crate::image_ref::ImageRef;
use crate::labels::isengard_enabled;
use crate::registry::RegistryClient;
use crate::tag_cache::TagCache;

/// Stable plugin name surfaced to the controller and host registry.
const PLUGIN_NAME: &str = "updater";

/// Default cycle interval when the config doesn't override it.
const DEFAULT_CYCLE_INTERVAL_SECS: u64 = 30;

/// Floor on the cycle interval; smaller values clamp up.
const MIN_CYCLE_INTERVAL_SECS: u64 = 5;

/// How long an approval row stays `pending_open` before the
/// controller's auto-expire task transitions it to
/// `pending_expired`.
const APPROVAL_DEFAULT_TTL_HOURS: i64 = 24;

/// Agent-side updater plugin instance.
///
/// Owns the docker handle, the registry client, the per-image tag
/// cache, and the wired-in host services (event emitter, dispatcher,
/// policy loader, approval store). The cycle task spawned from
/// [`Plugin::start`] reads everything via shared references.
pub struct Updater {
    /// Lazily set in `init`. Wrapped in `Option` so the struct can be
    /// constructed by the inventory factory before init runs.
    docker: Option<Docker>,
    /// Registry client (HEAD digest, tag list). Set in `init`.
    registry: Option<Arc<RegistryClient>>,
    /// Per-image tag cache for the `Minor` strategy.
    ///
    /// Built at plugin init with the default 1h TTL; shared across
    /// cycles so the per-cycle cost stays at one cache lookup per
    /// Minor candidate.
    tag_cache: Arc<TagCache>,
    /// Resolved cycle interval. Clamped up to
    /// [`MIN_CYCLE_INTERVAL_SECS`].
    cycle_interval: Duration,
    /// Optional event emitter wired from the agent.
    emitter: Option<Arc<dyn EventEmitter>>,
    /// Optional blue-green dispatcher.
    ///
    /// Set in `init` from `PluginContext::update_dispatcher`. When
    /// `Some`, the cycle consults it before recreating any non-self
    /// container: the dispatcher may take ownership and spawn a
    /// blue-green driver.
    dispatcher: Option<Arc<dyn UpdateDispatcher>>,
    /// Host id from `PluginContext::host_id`.
    ///
    /// Forwarded into every `UpdateTriggerInfo` so the dispatcher's
    /// downstream lookups (routing rules, deployment dedupe) target
    /// the right host.
    host_id: Option<HostId>,
    /// Policy loader from `PluginContext::policy_loader`.
    ///
    /// When `Some`, the cycle pulls the full policy snapshot at the
    /// start and resolves per-candidate (respects Pinned and
    /// `paused_until`).
    policy_loader: Option<Arc<dyn PolicyLoader>>,
    /// Approval store from `PluginContext::approval_store`.
    ///
    /// When `Some`, the cycle persists a pending-approval row
    /// whenever a candidate's resolved policy gates on `Approval`.
    /// `None` outside the agent or in test harnesses that don't
    /// exercise the approval path.
    approval_store: Option<Arc<dyn ApprovalStore>>,
    /// Cached fleet name for this host.
    ///
    /// Looked up once during `init` (when both a `policy_loader`
    /// and a `host_id` are wired) so the per-cycle path has zero
    /// extra DB hits. `None` means "no fleet-scoped rows match".
    fleet: Option<String>,
    /// Cancellation signal that ends the cycle task on `stop`.
    cancel: Arc<Notify>,
    /// Join handle for the spawned cycle task.
    task: Option<JoinHandle<()>>,
}

impl Updater {
    /// Builds an empty plugin. Wiring happens in [`Plugin::init`].
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
            tag_cache: Arc::new(TagCache::with_default_ttl()),
            cycle_interval: Duration::from_secs(DEFAULT_CYCLE_INTERVAL_SECS),
            emitter: None,
            dispatcher: None,
            host_id: None,
            policy_loader: None,
            approval_store: None,
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
// wrap them per-lifecycle-stage. Can refactor if a `From` impl lands
// in isengard-core.

/// Wraps any displayable error into [`CoreError::InitFailed`] for the
/// updater plugin.
fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Wraps any displayable error into [`CoreError::StartFailed`] for the
/// updater plugin.
fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

/// Forwards `event` to the wired emitter, if any.
async fn emit(emitter: Option<&Arc<dyn EventEmitter>>, event: Event) {
    if let Some(e) = emitter {
        e.emit(event).await;
    }
}

/// When the resolved strategy is `Minor`, list the registry's
/// tags via the cache, pick the highest patch+minor on the same major,
/// and (if strictly greater than the current tag) HEAD the bumped tag
/// to obtain its digest. Returns `(bumped_image_ref, bumped_digest)`
/// or `None` if no bump applies.
///
/// All errors are swallowed and degrade to `None`. Rationale: a transient
/// registry failure during tag listing should NOT escalate to a cycle
/// abort; the next cycle retries and the existing TagOnly digest path
/// still runs in the meantime.
pub async fn maybe_minor_bump(
    registry: &RegistryClient,
    tag_cache: &Arc<TagCache>,
    current: &ImageRef,
) -> Option<(ImageRef, String)> {
    let current_version = crate::tag_cache::parse_tag(&current.tag)?;
    let tags = match tag_cache
        .get_or_fetch(current, || async { registry.list_tags(current).await })
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(
                image = %current,
                error = %e,
                "tag-list fetch failed; minor strategy degraded to tag-only"
            );
            return None;
        }
    };
    let bumped = crate::tag_cache::pick_highest_minor(&tags, &current_version)?;
    // Reconstruct the tag string preserving the leading `v` if the running
    // image carried one, so `:1.2.3` stays `:1.3.0` and `:v1.2.3` becomes
    // `:v1.3.0`.
    let bumped_tag = if current.tag.starts_with('v') || current.tag.starts_with('V') {
        format!("v{bumped}")
    } else {
        bumped.to_string()
    };
    let bumped_ref = current.with_tag(&bumped_tag);
    let bumped_digest = match registry.head_digest(&bumped_ref).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(
                image = %bumped_ref,
                "minor bump tag exists in tags-list but registry returned no digest; skipping"
            );
            return None;
        }
        Err(e) => {
            warn!(
                image = %bumped_ref,
                error = %e,
                "minor bump HEAD failed; skipping"
            );
            return None;
        }
    };
    Some((bumped_ref, bumped_digest))
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
/// When `policy_loader` is `Some`, the cycle pulls the policy
/// snapshot once at the start, resolves a `ResolvedPolicy` per candidate
/// (using the candidate's compose labels + cached fleet + host_id), and
/// short-circuits with `update.policy_skipped` for `Pinned` services and
/// services with active `paused_until`. All other resolved-policy fields
/// are computed but NOT enforced; a later phase adds them.
#[allow(clippy::too_many_arguments)]
async fn do_cycle(
    docker: &Docker,
    registry: &RegistryClient,
    emitter: Option<&Arc<dyn EventEmitter>>,
    dispatcher: Option<&Arc<dyn UpdateDispatcher>>,
    host_id: Option<HostId>,
    policy_loader: Option<&Arc<dyn PolicyLoader>>,
    approval_store: Option<&Arc<dyn ApprovalStore>>,
    fleet: Option<&str>,
    tag_cache: &Arc<TagCache>,
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

    // Load the policy snapshot once per cycle. On loader error
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
    let mut deferred = 0usize;
    let mut pending_approval = 0usize;
    let mut pending_approval_dedup = 0usize;

    for c in &candidates {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let original_image_str = c.image.as_deref().unwrap_or("").to_string();
        // `image_str` is a borrowed alias that will be reassigned below if
        // the Minor strategy bumps the tag. The original string
        // (for `inspect_image`) lives in `original_image_str`.
        let mut image_str: String = original_image_str.clone();

        let Some(mut image_ref) = ImageRef::parse(&image_str) else {
            debug!(container = %name, image = %image_str, "skipping digest-pinned or unparseable image");
            continue;
        };

        // Policy gate: build the resolver context from the
        // candidate's compose labels + cached fleet + host_id, then
        // short-circuit on Pinned / paused_until. Gate=Approval
        // is enforced AFTER the registry probe (we need both digests to
        // build the approval body), so the early-skip pass passes
        // `approval_ctx=None`.
        let owned_ctx = crate::policy::policy_context_from_container(
            c.labels.as_ref(),
            fleet,
            host_id_hex.as_deref(),
            &name,
        );
        let projected = crate::policy::project_for_resolver(&policy_snapshot);
        let resolved = isengard_core::policy::resolve_policy(&projected, &owned_ctx.as_ref());
        match crate::policy::decision_from_resolved(
            &resolved,
            &owned_ctx.as_ref(),
            None,
            Utc::now(),
        ) {
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
                    crate::policy::SkipReason::GateRejected { reason } => {
                        info!(
                            container = %name,
                            reason = "gate_rejected",
                            gate_reason = ?reason,
                            "policy skip"
                        );
                        // Gate-rejected counts as a paused-style skip
                        // for cycle bookkeeping.
                        paused += 1;
                    }
                }
                emit(emitter, event).await;
                continue;
            }
            // Outside the maintenance window. Emit
            // `update.deferred` and skip recreation. The cycle moves on;
            // no approval row is persisted.
            crate::policy::PolicyDecision::Deferred { next_window } => {
                let when_str = next_window
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "unknown".to_string());
                info!(container = %name, next_window = %when_str, "policy deferred (outside window)");
                let event =
                    build_deferred_event(&owned_ctx, &name, host_id_hex.as_deref(), next_window);
                deferred += 1;
                emit(emitter, event).await;
                continue;
            }
            // PendingApproval is impossible here (`approval_ctx=None`
            // guarantees the gate=Approval branch falls through to
            // Proceed). The post-digest call below is where it materialises.
            crate::policy::PolicyDecision::Proceed
            | crate::policy::PolicyDecision::PendingApproval(_) => {}
        }

        // Local digest probe uses the ORIGINAL image string
        // (the tag actually running on this host). Even when the Minor
        // strategy bumps to a newer tag below, the local digest must
        // reflect what's running, not the proposed tag.
        let local_digest = match docker.inspect_image(&original_image_str).await {
            Ok(i) => i
                .repo_digests
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|d| d.split_once('@'))
                .map(|(_, dig)| dig.to_string()),
            Err(e) => {
                warn!(container = %name, image = %original_image_str, error = %e, "inspect_image failed");
                None
            }
        };

        let mut remote_digest = match registry.head_digest(&image_ref).await {
            Ok(d) => d,
            Err(e) => {
                warn!(container = %name, image = %image_str, error = %e, "registry HEAD failed");
                unknown += 1;
                continue;
            }
        };

        // Minor strategy: when the resolved policy is `Minor`,
        // additionally check the registry's tag list for a higher
        // patch+minor on the current major. If the picked tag is strictly
        // greater than the running tag AND has a different digest, we
        // override `image_ref` / `image_str` / `remote_digest` so the
        // downstream classification, approval, and recreate paths all
        // target the bumped tag.
        if resolved.strategy == isengard_core::policy::UpdateStrategy::Minor {
            if let Some((bumped_ref, bumped_digest)) =
                maybe_minor_bump(registry, tag_cache, &image_ref).await
            {
                info!(
                    container = %name,
                    from = %image_ref.tag,
                    to = %bumped_ref.tag,
                    "minor strategy: tag bump available"
                );
                image_str = bumped_ref.to_string();
                image_ref = bumped_ref;
                remote_digest = Some(bumped_digest);
            }
        }

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

                // Re-evaluate the resolved policy now that we
                // have both digests. If gate=Approval, the cycle MUST NOT
                // recreate; instead we persist a pending-approval row
                // (idempotently) and emit `update.pending_approval`. The
                // operator's eventual decide_pending_approval call queues
                // the apply_update HostAction that the agent picks up.
                let approval_ctx = crate::policy::ApprovalContext {
                    image: image_str.as_str(),
                    current_digest: local,
                    proposed_digest: remote,
                };
                let post_decision = crate::policy::decision_from_resolved(
                    &resolved,
                    &owned_ctx.as_ref(),
                    Some(approval_ctx),
                    Utc::now(),
                );
                if let crate::policy::PolicyDecision::PendingApproval(body) = post_decision {
                    let outcome = handle_pending_approval(
                        approval_store,
                        emitter,
                        body,
                        &name,
                        &image_str,
                        local,
                        remote,
                    )
                    .await;
                    match outcome {
                        ApprovalOutcome::Persisted => {
                            pending_approval += 1;
                            continue;
                        }
                        ApprovalOutcome::Deduplicated => {
                            pending_approval_dedup += 1;
                            continue;
                        }
                        ApprovalOutcome::PersistFailed => {
                            // Logged inside the helper. Do NOT recreate:
                            // we promised the operator a gate.
                            continue;
                        }
                        ApprovalOutcome::StoreUnavailable => {
                            // No store wired (test harness or pre-9e
                            // agent). Fall through to the legacy recreate
                            // path so the cycle stays useful in those
                            // contexts.
                            warn!(container = %name, "gate=Approval but no ApprovalStore wired; falling through to recreate");
                        }
                    }
                }

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
        up_to_date,
        needs_update,
        unknown,
        pinned,
        paused,
        deferred,
        pending_approval,
        pending_approval_dedup,
        "updater cycle complete"
    );

    emit(
        emitter,
        Event {
            kind: isengard_core::event::kinds::UPDATE_CHECKED.into(),
            occurred_at: Utc::now(),
            summary: format!(
                "cycle: candidates={} up_to_date={} needs_update={} unknown={} pinned={} paused={} deferred={} pending_approval={} pending_approval_dedup={}",
                candidates.len(),
                up_to_date,
                needs_update,
                unknown,
                pinned,
                paused,
                deferred,
                pending_approval,
                pending_approval_dedup,
            ),
            metadata: serde_json::json!({
                "candidates": candidates.len(),
                "up_to_date": up_to_date,
                "needs_update": needs_update,
                "unknown": unknown,
                "pinned": pinned,
                "paused": paused,
                "deferred": deferred,
                "pending_approval": pending_approval,
                "pending_approval_dedup": pending_approval_dedup,
            }),
            ..Default::default()
        },
    )
    .await;
    Ok(())
}

/// Outcome of the `gate=Approval` branch in `do_cycle`. Drives both the
/// per-candidate `continue`/`fall-through` decision and the cycle's
/// `pending_approval` / `pending_approval_dedup` counter increments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalOutcome {
    /// First-time persist succeeded; emitted `update.pending_approval`.
    Persisted,
    /// An open row already existed for this `(host, stack, service,
    /// proposed_digest)` tuple; no event, no insert.
    Deduplicated,
    /// `ApprovalStore` not wired on this `PluginContext`. The cycle falls
    /// through to the legacy recreate path so pre-9e harnesses still work.
    StoreUnavailable,
    /// Either the dedupe lookup or the insert returned an error. The
    /// helper logs the cause; the cycle skips this candidate to avoid
    /// honouring the gate by accident.
    PersistFailed,
}

/// Persist + emit a pending approval, idempotently.
///
/// Returns an [`ApprovalOutcome`] describing what the cycle should do
/// next. The function never panics; storage errors translate to
/// `PersistFailed`.
#[allow(clippy::too_many_arguments)]
async fn handle_pending_approval(
    approval_store: Option<&Arc<dyn ApprovalStore>>,
    emitter: Option<&Arc<dyn EventEmitter>>,
    body: isengard_core::PendingApprovalBody,
    container_name: &str,
    image: &str,
    current_digest: &str,
    proposed_digest: &str,
) -> ApprovalOutcome {
    let Some(store) = approval_store else {
        return ApprovalOutcome::StoreUnavailable;
    };

    // Dedup: did we already park this exact (host, stack, service,
    // proposed_digest) tuple? If yes, no event, no insert.
    match store
        .find_open_approval_for_proposed_digest(
            body.host_id,
            &body.stack,
            &body.service,
            &body.proposed_digest,
        )
        .await
    {
        Ok(Some(existing)) => {
            debug!(
                container = %container_name,
                action_id = %existing.action_id,
                proposed_digest = %proposed_digest,
                "pending_approval already open; dedup"
            );
            return ApprovalOutcome::Deduplicated;
        }
        Ok(None) => {}
        Err(e) => {
            warn!(
                container = %container_name,
                error = %e,
                "approval store dedup lookup failed; skipping recreate"
            );
            return ApprovalOutcome::PersistFailed;
        }
    }

    let approver_channel = body.approver_channel.clone();
    let host_id = body.host_id;
    let stack = body.stack.clone();
    let service = body.service.clone();
    let expires_at = Utc::now() + chrono::Duration::hours(APPROVAL_DEFAULT_TTL_HOURS);
    let ins = InsertPendingApproval {
        body,
        expires_at,
        approver_channel,
    };
    let rec = match store.insert_pending_approval(ins).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                container = %container_name,
                error = %e,
                "approval store insert failed; skipping recreate"
            );
            return ApprovalOutcome::PersistFailed;
        }
    };

    info!(
        container = %container_name,
        action_id = %rec.action_id,
        proposed_digest = %proposed_digest,
        "persisted pending_approval"
    );

    emit(
        emitter,
        Event {
            kind: isengard_core::event::kinds::UPDATE_PENDING_APPROVAL.into(),
            occurred_at: Utc::now(),
            summary: format!("pending approval for {container_name}"),
            container_name: Some(container_name.to_string()),
            image: Some(image.to_string()),
            old_digest: Some(current_digest.to_string()),
            new_digest: Some(proposed_digest.to_string()),
            metadata: serde_json::json!({
                "action_id": rec.action_id,
                "host_id": host_id.to_string(),
                "stack": stack,
                "service": service,
                "image": image,
                "current_digest": current_digest,
                "proposed_digest": proposed_digest,
            }),
            ..Default::default()
        },
    )
    .await;
    ApprovalOutcome::Persisted
}

/// Construct the `update.policy_skipped` event payload defined by the
/// spec:
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
    if let crate::policy::SkipReason::GateRejected { reason: Some(s) } = reason {
        payload["gate_reason"] = serde_json::Value::String(s.clone());
    }
    let summary = match reason {
        crate::policy::SkipReason::Pinned => format!("skipped {service} (pinned)"),
        crate::policy::SkipReason::Paused { until } => {
            format!("skipped {service} (paused until {})", until.to_rfc3339())
        }
        crate::policy::SkipReason::GateRejected { reason: Some(r) } => {
            format!("skipped {service} (gate rejected: {r})")
        }
        crate::policy::SkipReason::GateRejected { reason: None } => {
            format!("skipped {service} (gate rejected)")
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

/// Construct the `update.deferred` event payload:
///
/// ```json
/// {
///   "service": "...",
///   "container": "...",
///   "host_id": "...",
///   "next_window": "<RFC3339 UTC, optional>"
/// }
/// ```
///
/// `next_window` is omitted from the JSON when the cron has no future
/// occurrence (extremely rare; a malformed cron also lands here because
/// `is_in_window` returns false on parse error).
fn build_deferred_event(
    ctx: &crate::policy::OwnedPolicyContext,
    container_name: &str,
    host_id_hex: Option<&str>,
    next_window: Option<chrono::DateTime<Utc>>,
) -> Event {
    let service = ctx
        .service
        .clone()
        .unwrap_or_else(|| container_name.to_string());
    let mut payload = serde_json::json!({
        "service": service,
        "container": container_name,
        "host_id": host_id_hex.unwrap_or(""),
    });
    if let Some(t) = next_window {
        payload["next_window"] = serde_json::Value::String(t.to_rfc3339());
    }
    let summary = match next_window {
        Some(t) => format!("deferred {service} (next window {})", t.to_rfc3339()),
        None => format!("deferred {service} (next window unknown)"),
    };
    Event {
        kind: isengard_core::event::kinds::UPDATE_DEFERRED.into(),
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
        // Optional blue-green hand-off. When present, the cycle
        // consults the dispatcher before recreating any container.
        self.dispatcher = ctx.update_dispatcher.clone();
        if self.dispatcher.is_some() {
            info!("updater wired to update dispatcher (blue-green path enabled)");
        }
        self.host_id = ctx.host_id;

        // Pick up the approval store. When wired, the cycle
        // persists a pending-approval row for any candidate whose resolved
        // policy gates on `Approval`. `None` keeps the legacy recreate
        // path active for older agents and test harnesses.
        self.approval_store = ctx.approval_store.clone();
        if self.approval_store.is_some() {
            info!("updater wired to approval store (gate=Approval enforced)");
        }

        // Pick up the policy loader. When wired, the cycle
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
        let approval_store = self.approval_store.clone();
        let fleet = self.fleet.clone();
        let tag_cache = self.tag_cache.clone();
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
                            approval_store.as_ref(),
                            fleet.as_deref(),
                            &tag_cache,
                        ).await {
                            // Don't crash the task on a single bad cycle; log
                            // and try again next tick. Adds retry policy.
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
        // External-invocation entry point. Same as the internal task.
        // +: controller-triggered "force update now" lands here.
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
            self.approval_store.as_ref(),
            self.fleet.as_deref(),
            &self.tag_cache,
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

/// Compile-time assertion that [`Updater`] stays `Send + Sync`.
///
/// The inventory factory hands the plugin across threads.
#[allow(dead_code)]
fn _assert_send_sync() {
    /// Helper that fails to compile when `T` isn't `Send + Sync + 'static`.
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
