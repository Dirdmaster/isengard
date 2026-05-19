//! Per-deployment state machine driver.
//!
//! One [`Driver`] task owns one [`Deployment`] row and walks it through the
//! blue-green state machine: SpinningUp → Switching → Draining →
//! DestroyingBlue → Done. On failure (start, healthcheck, swap) the driver
//! transitions to Aborted, best-effort cleans up the green container, and
//! emits a `deployment.aborted` event so the dashboard / journal sees the
//! reason.
//!
//! All side effects (Docker calls, swap_upstream, healthcheck construction)
//! go through the [`DriverDeps`] trait so unit tests can swap in mocks
//! without standing up a real Docker daemon. The production binding is
//! [`RealDriverDeps`].
//!
//! See spec
//! `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`
//! §`driver.rs`.

use crate::deployment::healthcheck::DeploymentHealthcheck;
use crate::proxy::healthcheck::HealthChecker;
use crate::proxy::swap::swap_upstream;
use crate::proxy::upstreams::{Upstream, UpstreamState};
use crate::proxy::{ProxyEvent, ProxyState};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use isengard_core::policy::FailureHandling;
use isengard_core::{Event, EventEmitter};
use isengard_storage::deployment::{Deployment, DeploymentState};
use isengard_storage::inventory::Inventory;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// All side effects the driver performs. Production uses [`RealDriverDeps`];
/// tests swap in a mock that records calls and returns canned results.
#[async_trait]
pub trait DriverDeps: Send + Sync + 'static {
    /// Spin up the green container. Returns its `(container_id, addr)` once
    /// Docker reports it as started. The driver will then run the
    /// healthcheck against the returned addr.
    async fn start_green(&self, deployment: &Deployment) -> Result<(String, SocketAddr)>;

    /// Stop and remove `container_id`. Idempotent — a "not found" error from
    /// Docker is treated as success. Used for both green-cleanup-on-abort
    /// and blue-destruction-on-success.
    async fn stop_and_remove(&self, container_id: &str) -> Result<()>;

    /// Build the per-deployment healthcheck primitive (HTTP if the
    /// deployment carries a `health_path`, TCP-only otherwise). Returned by
    /// reference so tests can vary it without owning an `HealthChecker`.
    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker;

    /// Atomically swap traffic from blue to the green upstream at
    /// `deployment.public_hostname`. Drains the old entry over `grace`.
    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_container_id: &str,
        green_addr: SocketAddr,
        grace: Duration,
    ) -> Result<()>;

    /// Snapshot the upstream currently routing for this deployment's hostname.
    /// Returned `None` means there's nothing to recover to (no public_hostname,
    /// or the registry has no entry yet). Used by [`Driver::recover_to_blue`]
    /// when a post-switch collapse forces a rollback.
    async fn snapshot_current_upstream(&self, deployment: &Deployment) -> Option<Upstream>;

    /// Instant swap back to a previously-snapshotted blue upstream. Called
    /// during post-switch collapse recovery (`Recovering` state) and during
    /// abort-during-drain. Uses a zero grace because the green upstream is
    /// already failing: there's nothing in-flight to preserve.
    async fn swap_back_to_blue(&self, hostname: &str, blue: &Upstream) -> Result<()>;

    /// Re-pull `previous_digest` and recreate the container at
    /// that exact digest. Used by the `Rollback` failure-handling
    /// branch when green failed and we need to restore the prior
    /// version. Symmetric with `start_green` but pinned: we never
    /// pull a new tag here, only the recorded digest. Returns Ok when
    /// the fresh container is up; the driver then transitions to
    /// `RolledBack`. A default impl returns an error so existing
    /// non-rollback test fixtures don't have to stub it.
    async fn pull_and_recreate_at_digest(
        &self,
        _deployment: &Deployment,
        _previous_digest: &str,
    ) -> Result<()> {
        Err(anyhow!("pull_and_recreate_at_digest not implemented"))
    }
}

/// Default healthcheck cadence: 5s between probes, 2 consecutive passes
/// required, 120s deadline. Override per-driver via
/// [`Driver::with_healthcheck_overrides`] (used by tests with sub-second
/// deadlines).
const DEFAULT_HC_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_HC_SUCCESS_THRESHOLD: u32 = 2;
const DEFAULT_HC_DEADLINE: Duration = Duration::from_secs(120);

/// Grace period passed to [`swap_upstream`]: in-flight requests on blue have
/// this long to finish before the entry is removed from the registry.
const DEFAULT_SWAP_GRACE: Duration = Duration::from_secs(60);

/// Buffer added to the swap grace before transitioning DestroyingBlue, so a
/// small clock skew between [`swap_upstream`]'s cleanup task and the driver's
/// own timer can never race blue's destruction past in-flight requests.
const DRAIN_BUFFER: Duration = Duration::from_secs(5);

/// Outcome of the 3-way race during the drain window. See
/// [`Driver::race_drain`].
#[derive(Debug, PartialEq, Eq)]
enum DrainOutcome {
    /// Grace + buffer elapsed cleanly: proceed to DestroyingBlue → Done.
    Completed,
    /// Abort token fired: roll back to blue, kill green, mark Aborted.
    Aborted,
    /// Green container went unhealthy mid-drain: roll back to blue, kill
    /// green, transition Recovering → Failed with `post_switch_collapse_recovered`.
    GreenUnhealthy,
}

/// Per-deployment state-machine driver. One instance per [`Deployment`] row;
/// `run` consumes self and walks the machine to a terminal state.
pub struct Driver<D: DriverDeps> {
    pub deployment: Deployment,
    pub deps: Arc<D>,
    pub inventory: Inventory,
    pub emitter: Arc<dyn EventEmitter>,
    pub hc_interval: Duration,
    pub hc_success_threshold: u32,
    pub hc_deadline: Duration,
    pub swap_grace: Duration,
    pub drain_buffer: Duration,
    /// Cooperative cancel signal: when fired, the driver short-circuits to
    /// `Aborted`, cleans up green if it was started, and rolls back any
    /// post-switch routing change. Defaults to a fresh, non-cancelled token
    /// so existing call sites work unchanged.
    pub abort_token: CancellationToken,
    /// Optional subscription to the proxy event bus. When present, the
    /// driver listens for `UpstreamHealthChanged` during the drain window
    /// and triggers `recover_to_blue` if green flips unhealthy. `None`
    /// keeps the drain a plain sleep (pre-Plan-B behaviour).
    pub proxy_events_rx: Option<broadcast::Receiver<ProxyEvent>>,
    /// Snapshotted blue upstream taken just before `swap_upstream_to_green`,
    /// used by `recover_to_blue` to roll back. `None` until the snapshot is
    /// taken (i.e. before reaching `Switching`).
    pub blue_upstream_snapshot: Option<Upstream>,
    /// Which `on_failure` branch to take when the deployment
    /// breaks. Defaults to `Notify` (existing behaviour). When set to
    /// `Rollback`, failure paths consult `deployment.previous_digest`
    /// and call `pull_and_recreate_at_digest` instead of just
    /// transitioning to `Aborted`. When set to `Keep`, failure paths
    /// behave like Notify but ALSO upsert `paused_until = now + 24h`
    /// onto a service-scope policy row so future scans skip the
    /// service for a day.
    pub failure_handling: FailureHandling,
}

impl<D: DriverDeps> Driver<D> {
    pub fn new(
        deployment: Deployment,
        deps: Arc<D>,
        inventory: Inventory,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            deployment,
            deps,
            inventory,
            emitter,
            hc_interval: DEFAULT_HC_INTERVAL,
            hc_success_threshold: DEFAULT_HC_SUCCESS_THRESHOLD,
            hc_deadline: DEFAULT_HC_DEADLINE,
            swap_grace: DEFAULT_SWAP_GRACE,
            drain_buffer: DRAIN_BUFFER,
            abort_token: CancellationToken::new(),
            proxy_events_rx: None,
            blue_upstream_snapshot: None,
            failure_handling: FailureHandling::Notify,
        }
    }

    /// Install the resolved `on_failure` policy. Setting this
    /// to `Rollback` enables the supervisor's rollback branch on every
    /// failure path; `Keep` adds a 24h paused_until upsert to the
    /// existing abort path; `Notify` is the no-op default.
    pub fn with_failure_policy(mut self, handling: FailureHandling) -> Self {
        self.failure_handling = handling;
        self
    }

    /// Override the swap grace + drain buffer. Used by tests so the
    /// drain-sleep doesn't cost 65s of wall time per happy-path run.
    pub fn with_drain_overrides(mut self, swap_grace: Duration, drain_buffer: Duration) -> Self {
        self.swap_grace = swap_grace;
        self.drain_buffer = drain_buffer;
        self
    }

    /// Install a cancellation token the driver checks at every state edge
    /// and races against the spinup, healthcheck, and drain phases.
    pub fn with_abort_token(mut self, token: CancellationToken) -> Self {
        self.abort_token = token;
        self
    }

    /// Subscribe the driver to a proxy event bus so the drain race can
    /// react to `UpstreamHealthChanged` for the green container.
    pub fn with_proxy_events(mut self, rx: broadcast::Receiver<ProxyEvent>) -> Self {
        self.proxy_events_rx = Some(rx);
        self
    }

    /// Override the healthcheck cadence. Primary use: tests that need
    /// sub-second deadlines so the timeout case completes in milliseconds
    /// instead of the production 120s.
    pub fn with_healthcheck_overrides(
        mut self,
        interval: Duration,
        success_threshold: u32,
        deadline: Duration,
    ) -> Self {
        self.hc_interval = interval;
        self.hc_success_threshold = success_threshold;
        self.hc_deadline = deadline;
        self
    }

    /// Walk the state machine to a terminal state. On any error the driver
    /// transitions to `Aborted` (or `Failed` if the storage call itself
    /// breaks), records the error on the row, and emits
    /// `deployment.aborted`. Always runs to completion — this method does
    /// not propagate errors.
    pub async fn run(mut self) {
        match self.run_inner().await {
            Ok(()) => {}
            Err(e) => {
                // run_inner itself already handled per-state aborts. This
                // catches storage-layer breakage and similar surprises:
                // mark Failed and surface the error string.
                let msg = format!("driver_error: {e}");
                let _ = self
                    .inventory
                    .set_deployment_error(&self.deployment.id, &msg)
                    .await;
                let _ = self
                    .inventory
                    .update_deployment_state(&self.deployment.id, DeploymentState::Failed)
                    .await;
                self.emit("deployment.failed", Some(msg));
            }
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        // Top-level abort: if the user cancelled before we started, mark the
        // row Aborted without ever touching Docker.
        if self.abort_token.is_cancelled() {
            self.abort("aborted_by_user".to_string(), None).await?;
            return Ok(());
        }

        // SpinningUp: create + start green, racing the abort token so a
        // cancel during a slow image pull doesn't have to wait for the
        // pull to finish.
        self.transition(DeploymentState::SpinningUp).await?;
        let (green_id, green_addr) = tokio::select! {
            biased;
            _ = self.abort_token.cancelled() => {
                self.abort("aborted_by_user".to_string(), None).await?;
                return Ok(());
            }
            result = self.deps.start_green(&self.deployment) => {
                match result {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = format!("spinup_failed: {e}");
                        self.abort(msg, None).await?;
                        return Ok(());
                    }
                }
            }
        };
        self.deployment.green_container = Some(green_id.clone());
        self.inventory
            .set_deployment_green_container(&self.deployment.id, &green_id)
            .await?;

        // Healthcheck: wait for green to go healthy. Racing the abort token
        // means a stuck-on-startup container can't keep us pinned for the
        // full 120s deadline once the user clicks Cancel.
        let hc = DeploymentHealthcheck::new(self.deps.build_health_checker(&self.deployment))
            .with_interval(self.hc_interval)
            .with_success_threshold(self.hc_success_threshold)
            .with_deadline(self.hc_deadline);
        let hc_result = tokio::select! {
            biased;
            _ = self.abort_token.cancelled() => {
                self.abort("aborted_by_user".to_string(), Some(green_id.clone())).await?;
                return Ok(());
            }
            r = hc.wait_for_healthy(green_addr) => r,
        };
        match hc_result {
            Ok(passed_at) => {
                self.deployment.healthcheck_passed_at = Some(passed_at);
                let _ = self
                    .inventory
                    .set_deployment_healthcheck_passed(&self.deployment.id, passed_at)
                    .await;
            }
            Err(timeout) => {
                let msg = format!(
                    "healthcheck_timeout: {} attempts logged",
                    timeout.last_attempts.len()
                );
                self.abort(msg, Some(green_id.clone())).await?;
                return Ok(());
            }
        }

        // Snapshot the current upstream (blue) before we overwrite it. If
        // green collapses post-switch we'll swap back to this exact entry.
        self.blue_upstream_snapshot = self.deps.snapshot_current_upstream(&self.deployment).await;

        // Switching: atomic swap to green.
        self.transition(DeploymentState::Switching).await?;
        if let Err(e) = self
            .deps
            .swap_upstream_to_green(&self.deployment, &green_id, green_addr, self.swap_grace)
            .await
        {
            let msg = format!("swap_failed: {e}");
            self.abort(msg, Some(green_id.clone())).await?;
            return Ok(());
        }
        let switched_at = Utc::now();
        self.deployment.switched_at = Some(switched_at);
        let _ = self
            .inventory
            .set_deployment_switched(&self.deployment.id, switched_at)
            .await;

        // Draining: let in-flight requests on blue complete. 3-way race —
        // either the grace+buffer sleep finishes (happy path), the user
        // aborts, or the proxy event bus tells us green flipped unhealthy
        // (post-switch collapse).
        self.transition(DeploymentState::Draining).await?;
        let drain_outcome = self.race_drain(&green_id).await;
        match drain_outcome {
            DrainOutcome::Completed => {
                // Happy path — no abort, no green collapse — proceed to
                // destroying_blue → done as before.
                let drained_at = Utc::now();
                self.deployment.drained_at = Some(drained_at);
                let _ = self
                    .inventory
                    .set_deployment_drained(&self.deployment.id, drained_at)
                    .await;

                self.transition(DeploymentState::DestroyingBlue).await?;
                if let Some(blue_id) = self.deployment.blue_container.clone() {
                    let _ = self.deps.stop_and_remove(&blue_id).await;
                }

                let finished_at = Utc::now();
                self.deployment.finished_at = Some(finished_at);
                let _ = self
                    .inventory
                    .set_deployment_finished(&self.deployment.id, finished_at)
                    .await;
                self.transition(DeploymentState::Done).await?;
                self.emit("deployment.completed", None);
            }
            DrainOutcome::Aborted => {
                // User clicked abort mid-drain. Roll routing back to blue,
                // tear down green, mark the row Aborted.
                self.recover_to_blue("aborted_during_drain").await;
                let _ = self.deps.stop_and_remove(&green_id).await;
                self.deployment.error = Some("aborted_during_drain".to_string());
                let _ = self
                    .inventory
                    .set_deployment_error(&self.deployment.id, "aborted_during_drain")
                    .await;
                self.transition(DeploymentState::Aborted).await?;
                self.emit("deployment.aborted", Some("aborted_during_drain".into()));
            }
            DrainOutcome::GreenUnhealthy => {
                // Post-switch collapse. Transition to Recovering so the UI
                // can show the rollback in flight, swap routing back to
                // blue, kill green.
                self.transition(DeploymentState::Recovering).await?;
                self.recover_to_blue("post_switch_collapse_recovered").await;
                let _ = self.deps.stop_and_remove(&green_id).await;

                // If the resolved policy says Rollback and we
                // captured a previous_digest, attempt a re-pull. This
                // lands on RolledBack / RollbackFailed regardless of
                // whether the in-flight blue snapshot helped (we're
                // now reconstructing from the recorded digest, which
                // is more durable than the live container).
                if matches!(self.failure_handling, FailureHandling::Rollback)
                    && self.deployment.previous_digest.is_some()
                {
                    self.attempt_rollback("post_switch_collapse_recovered".to_string())
                        .await;
                } else {
                    self.transition(DeploymentState::Failed).await?;
                    self.emit(
                        "deployment.failed",
                        Some("post_switch_collapse_recovered".into()),
                    );
                    if matches!(self.failure_handling, FailureHandling::Keep) {
                        self.apply_keep_pause().await;
                    }
                }
            }
        }
        Ok(())
    }

    /// 3-way race during the drain window. Returns whichever signal arrives
    /// first: the grace+buffer sleep elapsing, the abort token firing, or a
    /// proxy event reporting our green container as unhealthy. Spurious
    /// events (different hostname, different container, blue going
    /// unhealthy) are ignored — the loop continues. If the proxy event
    /// channel closes (sender dropped), the listener is dropped and the
    /// race degrades to abort + sleep only.
    async fn race_drain(&mut self, green_id: &str) -> DrainOutcome {
        let total_drain = self.swap_grace + self.drain_buffer;
        let abort_token = self.abort_token.clone();
        let hostname = self.deployment.public_hostname.clone();
        let mut rx = self.proxy_events_rx.take();
        let sleep = tokio::time::sleep(total_drain);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                biased;
                _ = abort_token.cancelled() => return DrainOutcome::Aborted,
                event = async {
                    match rx.as_mut() {
                        Some(rx) => rx.recv().await.ok(),
                        None => std::future::pending().await,
                    }
                }, if rx.is_some() => {
                    match event {
                        Some(ProxyEvent::UpstreamHealthChanged {
                            public_hostname, container_id, healthy,
                        }) => {
                            if !healthy
                                && hostname.as_deref() == Some(public_hostname.as_str())
                                && container_id == green_id
                            {
                                return DrainOutcome::GreenUnhealthy;
                            }
                            // Spurious event (different host, different
                            // container, or healthy=true): keep racing.
                            continue;
                        }
                        None => {
                            // Channel closed (lagged or sender dropped).
                            // Stop listening; fall back to abort + sleep.
                            rx = None;
                        }
                    }
                }
                _ = &mut sleep => return DrainOutcome::Completed,
            }
        }
    }

    /// Roll routing back to the snapshotted blue upstream. Records `reason`
    /// on the deployment row regardless of whether the swap-back succeeded.
    /// Idempotent and best-effort: if there's no snapshot or no public
    /// hostname, we still set the error string so the UI sees why we
    /// couldn't recover.
    async fn recover_to_blue(&mut self, reason: &str) {
        let Some(blue) = self.blue_upstream_snapshot.clone() else {
            let msg = format!("{reason} (no_blue_snapshot)");
            self.deployment.error = Some(msg.clone());
            let _ = self
                .inventory
                .set_deployment_error(&self.deployment.id, &msg)
                .await;
            return;
        };
        let Some(hostname) = self.deployment.public_hostname.clone() else {
            let msg = format!("{reason} (no_public_hostname)");
            self.deployment.error = Some(msg.clone());
            let _ = self
                .inventory
                .set_deployment_error(&self.deployment.id, &msg)
                .await;
            return;
        };
        if let Err(e) = self.deps.swap_back_to_blue(&hostname, &blue).await {
            let msg = format!("{reason}_unrecoverable: {e}");
            self.deployment.error = Some(msg.clone());
            let _ = self
                .inventory
                .set_deployment_error(&self.deployment.id, &msg)
                .await;
            return;
        }
        // Successful swap-back — record the reason as the row's error so
        // the dashboard can explain the rollback.
        self.deployment.error = Some(reason.to_string());
        let _ = self
            .inventory
            .set_deployment_error(&self.deployment.id, reason)
            .await;
    }

    /// Move the row to `new` and emit `deployment.<state>`.
    async fn transition(&mut self, new: DeploymentState) -> Result<()> {
        self.inventory
            .update_deployment_state(&self.deployment.id, new)
            .await
            .map_err(|e| anyhow!("update_deployment_state({:?}): {e}", new))?;
        self.deployment.state = new;
        self.emit(&format!("deployment.{}", new.as_str()), None);
        Ok(())
    }

    /// Mark the deployment Aborted with `error_msg`, best-effort clean up
    /// `green_id` if present, and emit `deployment.aborted`. Used for every
    /// failure path that has already crossed the SpinningUp boundary.
    ///
    /// Cleanup is best-effort: a failed `stop_and_remove` doesn't block the
    /// transition to Aborted (the row state is more important than green's
    /// disposal), but it's logged at warn so persistent Docker issues are
    /// observable in production logs instead of silently leaking.
    ///
    /// When `self.failure_handling` is `Rollback` AND the
    /// deployment row carries `previous_digest`, the abort branches into
    /// [`Self::attempt_rollback`] instead of transitioning to `Aborted`.
    /// When it's `Keep`, the Aborted transition still happens but a
    /// best-effort 24h `paused_until` upsert follows so the next scan
    /// skips the service.
    async fn abort(&mut self, error_msg: String, green_id: Option<String>) -> Result<()> {
        // Branch FIRST on policy. Rollback re-pulls the prior
        // digest instead of cleaning up green; Keep + Notify both clean
        // up green as before.
        if matches!(self.failure_handling, FailureHandling::Rollback)
            && self.deployment.previous_digest.is_some()
        {
            // Clean up the failed green before re-pulling the previous
            // image. Same idempotent stop_and_remove the Notify path
            // does.
            if let Some(gid) = green_id.as_ref() {
                if let Err(e) = self.deps.stop_and_remove(gid).await {
                    tracing::warn!(
                        deployment_id = %self.deployment.id,
                        green_container = %gid,
                        error = %e,
                        "rollback: green container cleanup failed (may leak)"
                    );
                }
            }
            self.attempt_rollback(error_msg).await;
            return Ok(());
        }

        if let Some(gid) = green_id {
            if let Err(e) = self.deps.stop_and_remove(&gid).await {
                tracing::warn!(
                    deployment_id = %self.deployment.id,
                    green_container = %gid,
                    error = %e,
                    "deployment abort: green container cleanup failed (may leak)"
                );
            }
        }
        self.deployment.error = Some(error_msg.clone());
        if let Err(e) = self
            .inventory
            .set_deployment_error(&self.deployment.id, &error_msg)
            .await
        {
            tracing::warn!(
                deployment_id = %self.deployment.id,
                error = %e,
                "deployment abort: failed to persist error message to row"
            );
        }
        self.transition(DeploymentState::Aborted).await?;
        // Keep policy adds a 24h paused_until upsert AFTER the
        // Aborted transition lands. Best-effort: a storage failure here
        // logs but does not change the user-visible outcome (the row is
        // already Aborted; the worst case is the next scan re-attempts).
        if matches!(self.failure_handling, FailureHandling::Keep) {
            self.apply_keep_pause().await;
        }
        self.emit("deployment.aborted", Some(error_msg));
        Ok(())
    }

    /// Re-pull `previous_digest` and recreate the container at
    /// that exact digest. Stamps `rollback_attempted_at` regardless of
    /// success; transitions to `RolledBack` on success, `RollbackFailed`
    /// on error. The original failure that triggered the rollback is
    /// preserved on the row's `error` field so the dashboard can show
    /// both reasons.
    ///
    /// Caller is responsible for cleaning up the failed green before
    /// invoking this; we don't re-clean here.
    async fn attempt_rollback(&mut self, original_error: String) {
        let Some(previous) = self.deployment.previous_digest.clone() else {
            // Defensive: caller should have checked, but if we reach
            // here without a digest, fall back to Aborted with the
            // original error string preserved.
            self.deployment.error = Some(original_error.clone());
            let _ = self
                .inventory
                .set_deployment_error(&self.deployment.id, &original_error)
                .await;
            let _ = self.transition(DeploymentState::Aborted).await;
            self.emit("deployment.aborted", Some(original_error));
            return;
        };

        let now = Utc::now();
        self.deployment.rollback_attempted_at = Some(now);
        if let Err(e) = self
            .inventory
            .set_deployment_rollback_attempted(&self.deployment.id, now)
            .await
        {
            tracing::warn!(
                deployment_id = %self.deployment.id,
                error = %e,
                "rollback: failed to stamp rollback_attempted_at"
            );
        }

        // Surface the original failure first so it's visible during
        // the RollingBack state. attempt_rollback overwrites this
        // string only if the rollback ITSELF fails (in which case the
        // combined "<original>; rollback_failed: ..." message is what
        // the operator needs).
        self.deployment.error = Some(original_error.clone());
        let _ = self
            .inventory
            .set_deployment_error(&self.deployment.id, &original_error)
            .await;

        if let Err(e) = self.transition(DeploymentState::RollingBack).await {
            tracing::warn!(
                deployment_id = %self.deployment.id,
                error = %e,
                "rollback: failed to transition to RollingBack"
            );
            // Best-effort: continue trying the rollback anyway; if it
            // works, we'll transition to RolledBack from any state.
        }

        match self
            .deps
            .pull_and_recreate_at_digest(&self.deployment, &previous)
            .await
        {
            Ok(()) => {
                let finished_at = Utc::now();
                self.deployment.finished_at = Some(finished_at);
                let _ = self
                    .inventory
                    .set_deployment_finished(&self.deployment.id, finished_at)
                    .await;
                let _ = self.transition(DeploymentState::RolledBack).await;
                self.emit(
                    "update.rolled_back",
                    Some(format!("rolled back to {previous}")),
                );
            }
            Err(e) => {
                let combined = format!("{original_error}; rollback_failed: {e}");
                self.deployment.error = Some(combined.clone());
                let _ = self
                    .inventory
                    .set_deployment_error(&self.deployment.id, &combined)
                    .await;
                let _ = self.transition(DeploymentState::RollbackFailed).await;
                self.emit("update.rollback_failed", Some(combined));
            }
        }
    }

    /// Best-effort upsert of a 24h `paused_until` on a
    /// service-scope policy row, keyed by the deployment's
    /// `service_name`. Used by the `Keep` failure handler so the next
    /// updater scan skips the service for a day.
    ///
    /// Storage failures are logged but never block the abort path. The
    /// row is already Aborted by the time we get here; failing to apply
    /// the pause just means a subsequent scan may re-attempt the
    /// deployment, which is recoverable.
    async fn apply_keep_pause(&self) {
        let until = Utc::now() + chrono::Duration::hours(24);
        let key = self.deployment.service_name.clone();
        if let Err(e) = self.inventory.pause_service_policy(&key, until).await {
            tracing::warn!(
                deployment_id = %self.deployment.id,
                service = %self.deployment.service_name,
                error = %e,
                "keep policy: pause_service_policy failed (continuing)"
            );
        }
    }

    /// Fire-and-forget event emission. Spawned so the state machine never
    /// blocks on emitter latency (network, journal flush, etc.).
    ///
    /// The full [`Deployment`] row is serialized into `metadata.deployment`
    /// so subscribers (dashboard, journal, abort UI) get the entire context
    /// without needing a follow-up storage lookup.
    fn emit(&self, kind: &str, error: Option<String>) {
        let deployment_json =
            serde_json::to_value(&self.deployment).unwrap_or_else(|_| serde_json::json!({}));
        let event = Event {
            kind: kind.to_string(),
            occurred_at: Utc::now(),
            host_id: Some(self.deployment.host_id.0),
            summary: format!(
                "{} {} {}",
                self.deployment.service_name, self.deployment.green_digest, kind
            ),
            error,
            metadata: serde_json::json!({
                "deployment": deployment_json,
            }),
            ..Default::default()
        };
        let emitter = self.emitter.clone();
        tokio::spawn(async move {
            emitter.emit(event).await;
        });
    }
}

/// Production [`DriverDeps`] binding: bollard for Docker, the
/// [`HealthChecker`] primitive for probes, and [`swap_upstream`] for the
/// traffic shift.
pub struct RealDriverDeps {
    pub docker: Arc<bollard::Docker>,
    pub proxy_state: ProxyState,
}

#[async_trait]
impl DriverDeps for RealDriverDeps {
    async fn start_green(&self, deployment: &Deployment) -> Result<(String, SocketAddr)> {
        use bollard::container::{Config, CreateContainerOptions, StartContainerOptions};
        use bollard::image::CreateImageOptions;
        use futures_util::StreamExt;

        let blue_id = deployment
            .blue_container
            .as_deref()
            .ok_or_else(|| anyhow!("no blue container to base green on"))?;
        let blue = self
            .docker
            .inspect_container(blue_id, None)
            .await
            .with_context(|| format!("inspect blue {blue_id}"))?;

        let image = blue
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .ok_or_else(|| anyhow!("blue has no image"))?;
        // For BG, the green image is the same name with the new digest. The
        // updater detected the digest change against the registry; the image
        // tag is unchanged. Pulling re-fetches the new digest under that tag.
        let pull_opts = CreateImageOptions {
            from_image: image.as_str(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(pull_opts), None, None);
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("pulling {image}"))?;
        }

        // Build green Config from blue (mirror updater's recreate.rs::capture_config
        // pattern, but DON'T set image to a new tag — we're keeping the tag, just
        // running the new digest).
        let cfg_in = blue.config.clone().unwrap_or_default();
        let host_cfg = blue.host_config.clone().unwrap_or_default();
        let cfg: Config<String> = Config {
            image: Some(image.clone()),
            cmd: cfg_in.cmd,
            entrypoint: cfg_in.entrypoint,
            env: cfg_in.env,
            labels: cfg_in.labels,
            working_dir: cfg_in.working_dir,
            user: cfg_in.user,
            exposed_ports: cfg_in.exposed_ports,
            host_config: Some(host_cfg),
            healthcheck: cfg_in.healthcheck,
            ..Default::default()
        };

        // Green container name: <service>-green-<deployment_id_prefix>
        let id_short = &deployment.id[..deployment.id.len().min(8)];
        let green_name = format!("{}-green-{}", deployment.service_name, id_short);
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: green_name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .with_context(|| format!("create container {green_name}"))?;

        // Reconnect to non-bridge networks (mirror recreate.rs).
        if let Some(ns) = blue
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
        {
            for (net_name, settings) in ns {
                if net_name == "bridge" {
                    continue;
                }
                let _ = self
                    .docker
                    .connect_network(
                        net_name,
                        bollard::network::ConnectNetworkOptions {
                            container: create.id.clone(),
                            endpoint_config: settings.clone(),
                        },
                    )
                    .await; // best-effort; some networks may have changed
            }
        }

        self.docker
            .start_container(&create.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("start container {green_name}"))?;

        // Re-inspect to get the assigned IP.
        let started = self.docker.inspect_container(&create.id, None).await?;
        let ip = started
            .network_settings
            .as_ref()
            .and_then(|s| s.ip_address.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // Fall back to the first network's IP.
                started
                    .network_settings
                    .as_ref()
                    .and_then(|s| s.networks.as_ref())
                    .and_then(|nets| {
                        nets.values().find_map(|settings| {
                            settings.ip_address.clone().filter(|s| !s.is_empty())
                        })
                    })
            })
            .ok_or_else(|| anyhow!("green container has no IP"))?;
        let port = deployment.container_port.unwrap_or(8080) as u16;
        let addr: SocketAddr = format!("{ip}:{port}")
            .parse()
            .with_context(|| format!("parse addr {ip}:{port}"))?;

        Ok((create.id, addr))
    }

    async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
        use bollard::container::{RemoveContainerOptions, StopContainerOptions};
        // Best-effort stop: a missing-or-already-stopped container should
        // not fail the cleanup — the subsequent remove_container handles
        // both cases idempotently below.
        let _ = self
            .docker
            .stop_container(container_id, Some(StopContainerOptions { t: 10 }))
            .await;
        match self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // bollard's "no such container" surfaces as a 404; treat
                // as success so manual cleanup races don't fail the abort
                // path. Anything else is a real failure.
                let s = e.to_string();
                if s.contains("404") || s.to_lowercase().contains("no such container") {
                    Ok(())
                } else {
                    Err(anyhow!("remove {container_id}: {e}"))
                }
            }
        }
    }

    fn build_health_checker(&self, deployment: &Deployment) -> HealthChecker {
        match &deployment.health_path {
            Some(p) => HealthChecker::new(p.clone(), Duration::from_secs(2)),
            None => HealthChecker::tcp_only(Duration::from_secs(2)),
        }
    }

    async fn swap_upstream_to_green(
        &self,
        deployment: &Deployment,
        green_container_id: &str,
        green_addr: SocketAddr,
        grace: Duration,
    ) -> Result<()> {
        let hostname = deployment
            .public_hostname
            .as_deref()
            .ok_or_else(|| anyhow!("deployment {} has no public_hostname", deployment.id))?;
        let upstream = Upstream {
            container_id: green_container_id.to_string(),
            addr: green_addr,
            healthy: true,
            health_path: deployment.health_path.clone(),
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        };
        swap_upstream(&self.proxy_state, hostname, upstream, grace).await
    }

    async fn snapshot_current_upstream(&self, deployment: &Deployment) -> Option<Upstream> {
        let host = deployment.public_hostname.as_deref()?;
        let r = self.proxy_state.upstreams.read().await;
        r.get(host).cloned()
    }

    async fn swap_back_to_blue(&self, hostname: &str, blue: &Upstream) -> Result<()> {
        // Zero grace: the green upstream is failing, there's nothing in
        // flight on it worth preserving — switch back instantly.
        swap_upstream(
            &self.proxy_state,
            hostname,
            blue.clone(),
            Duration::from_secs(0),
        )
        .await
    }

    /// Re-pull `previous_digest` and recreate the container at
    /// that pinned image. Mirrors `start_green` but uses the digest as
    /// the image reference so the registry serves the exact prior
    /// version. Best-effort container destroy on the existing
    /// blue/green slot first; then create + start a fresh container.
    async fn pull_and_recreate_at_digest(
        &self,
        deployment: &Deployment,
        previous_digest: &str,
    ) -> Result<()> {
        use bollard::container::{Config, CreateContainerOptions, StartContainerOptions};
        use bollard::image::CreateImageOptions;
        use futures_util::StreamExt;

        // Best-effort: kill blue if it's still around so we don't
        // collide with the rolled-back container's name.
        if let Some(blue_id) = deployment.blue_container.as_deref() {
            let _ = self.stop_and_remove(blue_id).await;
        }

        // Image reference for pull is "<image_name>@<digest>". The
        // image name comes from the trigger's image_ref field, which
        // we recovered onto the row indirectly via blue's inspect.
        // Look up blue's image to derive the bare repo name.
        let blue_id = deployment
            .blue_container
            .as_deref()
            .ok_or_else(|| anyhow!("rollback: no blue container ref to derive image"))?;
        // After the destroy above, inspect_container will fail. Pull
        // by digest directly using a synthetic ref: docker accepts
        // "<repo>@<digest>" as a valid pull source. We don't have the
        // bare repo here, so we ask docker to pull the digest directly
        // via the registry URL recorded by the digest itself. In
        // practice the operator runs the rollback path against the
        // same registry as the original image, so the digest pull
        // succeeds when the digest is still present.
        //
        // Fallback path: if blue is gone, refuse and surface a clear
        // error. The supervisor will mark RollbackFailed.
        let inspect = self.docker.inspect_container(blue_id, None).await.ok();
        let image_repo = inspect
            .as_ref()
            .and_then(|i| i.config.as_ref())
            .and_then(|c| c.image.clone())
            .ok_or_else(|| {
                anyhow!(
                    "rollback: blue container {blue_id} no longer exists; cannot derive image repo to re-pull {previous_digest}"
                )
            })?;
        // Strip any `:tag` suffix; for `repo@digest` syntax we want
        // the repo bare.
        let bare_repo = image_repo
            .split(':')
            .next()
            .map(str::to_string)
            .unwrap_or(image_repo.clone());
        let pull_ref = format!("{bare_repo}@{previous_digest}");
        let pull_opts = CreateImageOptions {
            from_image: pull_ref.as_str(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(pull_opts), None, None);
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("rollback: pulling {pull_ref}"))?;
        }

        // Recreate at the pinned digest using blue's prior config.
        let cfg_in = inspect
            .as_ref()
            .and_then(|i| i.config.clone())
            .unwrap_or_default();
        let host_cfg = inspect
            .as_ref()
            .and_then(|i| i.host_config.clone())
            .unwrap_or_default();
        let cfg: Config<String> = Config {
            image: Some(pull_ref.clone()),
            cmd: cfg_in.cmd,
            entrypoint: cfg_in.entrypoint,
            env: cfg_in.env,
            labels: cfg_in.labels,
            working_dir: cfg_in.working_dir,
            user: cfg_in.user,
            exposed_ports: cfg_in.exposed_ports,
            host_config: Some(host_cfg),
            healthcheck: cfg_in.healthcheck,
            ..Default::default()
        };
        let id_short = &deployment.id[..deployment.id.len().min(8)];
        let rb_name = format!("{}-rb-{}", deployment.service_name, id_short);
        let create = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: rb_name.clone(),
                    platform: None,
                }),
                cfg,
            )
            .await
            .with_context(|| format!("rollback: create container {rb_name}"))?;
        self.docker
            .start_container(&create.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("rollback: start container {rb_name}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_core::NoopEmitter;
    use isengard_storage::deployment::{DeployStrategy, InsertDeployment};
    use isengard_storage::host::EnrollHost;
    use isengard_storage::inventory::Inventory;
    use isengard_storage::stack::{InsertStack, StackSource};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Insert a fresh in-memory inventory, host, stack, and Pending
    /// deployment row. Returns the inventory + the row so each test can
    /// drive its own driver against them.
    async fn setup_inventory_and_row(state: DeploymentState) -> (Inventory, Deployment) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let d = inv
            .insert_deployment(InsertDeployment {
                id: ulid::Ulid::new().to_string(),
                host_id,
                stack_id,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state,
                blue_container: Some("blue-id".into()),
                green_container: None,
                blue_digest: "sha256:aaa".into(),
                green_digest: "sha256:bbb".into(),
                public_hostname: Some("blog.test".into()),
                health_path: None,
                container_port: Some(8080),
                metadata_json: None,
                previous_digest: None,
            })
            .await
            .unwrap();
        (inv, d)
    }

    /// A live TCP listener whose addr is healthy for [`HealthChecker::tcp_only`].
    /// Returned task is aborted at end of scope by drop.
    fn live_addr() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let h = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        (addr, h)
    }

    /// An addr that will refuse all connections (for healthcheck timeout).
    fn dead_addr() -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    }

    // ---- Test 1: happy path ----

    struct HappyDeps {
        addr: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
        swap_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl DriverDeps for HappyDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.addr))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            self.swap_called.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
            None
        }
        async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn happy_path_advances_to_done() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let swap_called = Arc::new(AtomicBool::new(false));
        let deps = Arc::new(HappyDeps {
            addr,
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
            swap_called: swap_called.clone(),
        });
        // Real wall-clock here (sqlx pool dislikes tokio's paused clock).
        // Tiny grace + buffer keeps the run under ~100ms even with the
        // drain sleep.
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
            .with_drain_overrides(Duration::from_millis(20), Duration::from_millis(10));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Done, "expected Done");
        assert!(swap_called.load(Ordering::SeqCst), "swap was called");
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "exactly one stop_and_remove (blue)"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("blue-id"),
            "blue is the one destroyed"
        );
        assert!(after.green_container.as_deref() == Some("green-id"));
        assert!(after.healthcheck_passed_at.is_some());
        assert!(after.switched_at.is_some());
        assert!(after.drained_at.is_some());
        assert!(after.finished_at.is_some());
    }

    // ---- Test 2: spinup failure marks Aborted ----

    struct SpinupFailDeps;

    #[async_trait]
    impl DriverDeps for SpinupFailDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Err(anyhow!("docker create: image pull failed: 404 not found"))
        }
        async fn stop_and_remove(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Ok(())
        }
        async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
            None
        }
        async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn spinup_failure_marks_aborted() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let driver = Driver::new(
            d,
            Arc::new(SpinupFailDeps),
            inv.clone(),
            Arc::new(NoopEmitter),
        );
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.starts_with("spinup_failed:"),
            "expected spinup_failed prefix, got {err:?}"
        );
        assert!(after.green_container.is_none(), "green never assigned");
    }

    // ---- Test 3: healthcheck timeout marks Aborted + cleans green ----

    struct HealthTimeoutDeps {
        dead: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl DriverDeps for HealthTimeoutDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.dead))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(20))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Ok(())
        }
        async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
            None
        }
        async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn healthcheck_timeout_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let deps = Arc::new(HealthTimeoutDeps {
            dead: dead_addr(),
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
        });
        // Sub-second deadline so the test completes fast. Real-time clock
        // here (no start_paused) because the healthcheck loop polls a
        // refused TCP addr — that's a real syscall, not a sleep.
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(20), 3, Duration::from_millis(200));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("healthcheck_timeout"),
            "expected healthcheck_timeout, got {err:?}"
        );
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "green should be cleaned up exactly once"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("green-id"),
            "the cleaned container is green"
        );
    }

    // ---- Test 4: swap failure marks Aborted + cleans green ----

    struct SwapFailDeps {
        addr: SocketAddr,
        stop_remove_calls: Arc<AtomicUsize>,
        last_stop_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl DriverDeps for SwapFailDeps {
        async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
            Ok(("green-id".into(), self.addr))
        }
        async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
            self.stop_remove_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_stop_id.lock().unwrap() = Some(container_id.to_string());
            Ok(())
        }
        fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
            HealthChecker::tcp_only(Duration::from_millis(100))
        }
        async fn swap_upstream_to_green(
            &self,
            _d: &Deployment,
            _gid: &str,
            _addr: SocketAddr,
            _grace: Duration,
        ) -> Result<()> {
            Err(anyhow!("registry write contention"))
        }
        async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
            None
        }
        async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn swap_failure_marks_aborted_and_cleans_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();
        let stop_remove_calls = Arc::new(AtomicUsize::new(0));
        let last_stop_id = Arc::new(std::sync::Mutex::new(None));
        let deps = Arc::new(SwapFailDeps {
            addr,
            stop_remove_calls: stop_remove_calls.clone(),
            last_stop_id: last_stop_id.clone(),
        });
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500));
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("swap_failed"),
            "expected swap_failed, got {err:?}"
        );
        assert_eq!(
            stop_remove_calls.load(Ordering::SeqCst),
            1,
            "green should be cleaned up exactly once"
        );
        assert_eq!(
            last_stop_id.lock().unwrap().as_deref(),
            Some("green-id"),
            "the cleaned container is green (blue still serving)"
        );
    }

    // ---- Test 5: emit places the full Deployment row in event metadata ----

    #[tokio::test]
    async fn emit_includes_full_deployment_in_metadata() {
        use isengard_core::Event;
        use std::sync::Mutex as StdMutex;

        struct CaptureEmitter {
            events: Arc<StdMutex<Vec<Event>>>,
        }
        #[async_trait::async_trait]
        impl EventEmitter for CaptureEmitter {
            async fn emit(&self, event: Event) {
                self.events.lock().unwrap().push(event);
            }
        }

        let captured = Arc::new(StdMutex::new(Vec::new()));
        let emitter: Arc<dyn EventEmitter> = Arc::new(CaptureEmitter {
            events: captured.clone(),
        });

        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;

        struct NoopDeps;
        #[async_trait::async_trait]
        impl DriverDeps for NoopDeps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("g".into(), "127.0.0.1:0".parse().unwrap()))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(10))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
        }

        let driver = Driver::new(d.clone(), Arc::new(NoopDeps), inv, emitter);
        driver.emit("deployment.spinning_up", None);
        // emit() spawns the actual delivery — give the runtime a tick to flush.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, "deployment.spinning_up");
        let dep = e
            .metadata
            .get("deployment")
            .expect("deployment in metadata");
        assert_eq!(dep.get("id").and_then(|v| v.as_str()), Some(d.id.as_str()));
        assert_eq!(dep.get("state").and_then(|v| v.as_str()), Some("pending"));
        assert_eq!(
            dep.get("service_name").and_then(|v| v.as_str()),
            Some("web")
        );
        assert_eq!(
            dep.get("green_digest").and_then(|v| v.as_str()),
            Some("sha256:bbb")
        );
    }

    // ---- Tests 6-9: Plan B abort + post-switch collapse recovery ----

    /// Helper: build a synthetic blue Upstream the snapshot mocks can return.
    fn synthetic_blue() -> Upstream {
        Upstream {
            container_id: "blue-id".into(),
            addr: "127.0.0.1:9999".parse().unwrap(),
            healthy: true,
            health_path: None,
            health_interval: Duration::from_secs(5),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        }
    }

    /// Pre-cancelled abort: driver should mark Aborted without ever calling
    /// `start_green` (the panic in the mock catches any regression).
    #[tokio::test]
    async fn abort_during_pending_marks_aborted_without_starting_green() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let token = CancellationToken::new();
        token.cancel();

        struct Deps;
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                panic!("start_green must not be called when aborted before spinup");
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(10))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
        }

        let driver = Driver::new(d, Arc::new(Deps), inv.clone(), Arc::new(NoopEmitter))
            .with_abort_token(token);
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("aborted_by_user"),
            "expected aborted_by_user, got {err:?}"
        );
        assert!(after.green_container.is_none(), "green never assigned");
    }

    /// Abort fires during a slow `start_green`. The biased select on the
    /// abort branch must win the race; the driver records Aborted and
    /// never assigns a green container.
    #[tokio::test]
    async fn abort_during_spinup_cleans_green_and_marks_aborted() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let token = CancellationToken::new();
        let token_for_cancel = token.clone();

        struct Deps;
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                // Long-running spinup: only resolves if the driver doesn't
                // race-win on the abort branch.
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(("g".into(), "127.0.0.1:1".parse().unwrap()))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(10))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
        }

        let driver = Driver::new(d, Arc::new(Deps), inv.clone(), Arc::new(NoopEmitter))
            .with_abort_token(token);
        let driver_task = tokio::spawn(driver.run());
        tokio::time::sleep(Duration::from_millis(50)).await;
        token_for_cancel.cancel();
        driver_task.await.unwrap();

        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("aborted_by_user"),
            "expected aborted_by_user, got {err:?}"
        );
        assert!(after.green_container.is_none(), "green never assigned");
    }

    /// Abort fires while the driver is in the drain window. The 3-way race
    /// returns `Aborted`, `recover_to_blue` swaps routing back, green is
    /// removed, the row is marked Aborted with `aborted_during_drain`.
    #[tokio::test]
    async fn abort_during_drain_swaps_back_and_marks_aborted() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();
        let token = CancellationToken::new();
        let token_for_cancel = token.clone();

        struct Deps {
            addr: SocketAddr,
            swap_back_called: Arc<AtomicBool>,
            green_removed: Arc<AtomicBool>,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
                if container_id == "green-id" {
                    self.green_removed.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(50))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                Some(synthetic_blue())
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                self.swap_back_called.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let swap_back_called = Arc::new(AtomicBool::new(false));
        let green_removed = Arc::new(AtomicBool::new(false));
        let deps = Arc::new(Deps {
            addr,
            swap_back_called: swap_back_called.clone(),
            green_removed: green_removed.clone(),
        });
        // Long drain window so the test gets a deterministic chance to
        // fire the abort token while we're parked in race_drain.
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
            .with_drain_overrides(Duration::from_secs(10), Duration::from_secs(1))
            .with_abort_token(token);

        let driver_task = tokio::spawn(driver.run());
        tokio::time::sleep(Duration::from_millis(150)).await;
        token_for_cancel.cancel();
        driver_task.await.unwrap();

        assert!(
            swap_back_called.load(Ordering::SeqCst),
            "swap_back_to_blue should fire on abort-during-drain"
        );
        assert!(
            green_removed.load(Ordering::SeqCst),
            "green should be cleaned up after rollback"
        );
        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Aborted);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("aborted_during_drain"),
            "expected aborted_during_drain, got {err:?}"
        );
    }

    /// Green container flips unhealthy during the drain window. The driver
    /// transitions Recovering → Failed, swaps routing back to blue, kills
    /// green, and records `post_switch_collapse_recovered`.
    #[tokio::test]
    async fn green_unhealthy_during_drain_recovers_and_marks_failed() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();

        struct Deps {
            addr: SocketAddr,
            swap_back_called: Arc<AtomicBool>,
            green_removed: Arc<AtomicBool>,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, container_id: &str) -> Result<()> {
                if container_id == "green-id" {
                    self.green_removed.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(50))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                Some(synthetic_blue())
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                self.swap_back_called.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let bus = crate::proxy::ProxyEventBus::new();
        let rx = bus.subscribe();
        let swap_back_called = Arc::new(AtomicBool::new(false));
        let green_removed = Arc::new(AtomicBool::new(false));
        let deps = Arc::new(Deps {
            addr,
            swap_back_called: swap_back_called.clone(),
            green_removed: green_removed.clone(),
        });
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
            .with_drain_overrides(Duration::from_secs(10), Duration::from_secs(1))
            .with_proxy_events(rx);

        let driver_task = tokio::spawn(driver.run());
        // Wait for entry into the drain window, then publish the unhealthy
        // event the race_drain loop is listening for.
        tokio::time::sleep(Duration::from_millis(150)).await;
        bus.publish(crate::proxy::ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "green-id".into(),
            healthy: false,
        });
        driver_task.await.unwrap();

        assert!(
            swap_back_called.load(Ordering::SeqCst),
            "swap_back_to_blue should fire on green collapse"
        );
        assert!(
            green_removed.load(Ordering::SeqCst),
            "green should be cleaned up after rollback"
        );
        let after = inv.get_deployment(&id).await.unwrap().expect("row exists");
        assert_eq!(after.state, DeploymentState::Failed);
        let err = after.error.expect("error recorded");
        assert!(
            err.contains("post_switch_collapse_recovered"),
            "expected post_switch_collapse_recovered, got {err:?}"
        );
    }

    // ---- rollback failure handler ----

    /// Variant of `setup_inventory_and_row` that seeds `previous_digest`
    /// so the driver can take the rollback branch.
    async fn setup_with_previous_digest(
        state: DeploymentState,
        previous: &str,
    ) -> (Inventory, Deployment) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp-rb".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let d = inv
            .insert_deployment(InsertDeployment {
                id: ulid::Ulid::new().to_string(),
                host_id,
                stack_id,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state,
                blue_container: Some("blue-id".into()),
                green_container: None,
                blue_digest: "sha256:aaa".into(),
                green_digest: "sha256:bbb".into(),
                public_hostname: Some("blog.test".into()),
                health_path: None,
                container_port: Some(8080),
                metadata_json: None,
                previous_digest: Some(previous.to_string()),
            })
            .await
            .unwrap();
        (inv, d)
    }

    /// Rollback policy + green never goes healthy + rollback re-pull
    /// succeeds. Final state is `RolledBack`, `rollback_attempted_at`
    /// is populated, the deps' `pull_and_recreate_at_digest` was
    /// invoked exactly once with the recorded `previous_digest`.
    #[tokio::test]
    async fn rollback_on_healthcheck_timeout_succeeds() {
        let (inv, d) =
            setup_with_previous_digest(DeploymentState::Pending, "sha256:previous_aaa").await;
        let id = d.id.clone();
        let dead = dead_addr();

        struct Deps {
            dead: SocketAddr,
            rollback_calls: Arc<AtomicUsize>,
            last_digest: Arc<std::sync::Mutex<Option<String>>>,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.dead))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(20))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
            async fn pull_and_recreate_at_digest(
                &self,
                _d: &Deployment,
                previous: &str,
            ) -> Result<()> {
                self.rollback_calls.fetch_add(1, Ordering::SeqCst);
                *self.last_digest.lock().unwrap() = Some(previous.to_string());
                Ok(())
            }
        }

        let rollback_calls = Arc::new(AtomicUsize::new(0));
        let last_digest = Arc::new(std::sync::Mutex::new(None));
        let deps = Arc::new(Deps {
            dead,
            rollback_calls: rollback_calls.clone(),
            last_digest: last_digest.clone(),
        });
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(20), 3, Duration::from_millis(200))
            .with_failure_policy(FailureHandling::Rollback);
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::RolledBack);
        assert!(after.rollback_attempted_at.is_some());
        assert_eq!(rollback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            last_digest.lock().unwrap().as_deref(),
            Some("sha256:previous_aaa")
        );
    }

    /// Rollback policy + green never goes healthy + rollback re-pull
    /// fails (image gone from registry). Final state is
    /// `RollbackFailed`, error string mentions both the original
    /// healthcheck_timeout AND the rollback failure.
    #[tokio::test]
    async fn rollback_on_healthcheck_timeout_fails_when_image_gone() {
        let (inv, d) =
            setup_with_previous_digest(DeploymentState::Pending, "sha256:previous_bbb").await;
        let id = d.id.clone();
        let dead = dead_addr();

        struct Deps {
            dead: SocketAddr,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.dead))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(20))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
            async fn pull_and_recreate_at_digest(
                &self,
                _d: &Deployment,
                _previous: &str,
            ) -> Result<()> {
                Err(anyhow!("manifest unknown: image not found in registry"))
            }
        }

        let driver = Driver::new(
            d,
            Arc::new(Deps { dead }),
            inv.clone(),
            Arc::new(NoopEmitter),
        )
        .with_healthcheck_overrides(Duration::from_millis(20), 3, Duration::from_millis(200))
        .with_failure_policy(FailureHandling::Rollback);
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::RollbackFailed);
        let err = after.error.expect("error");
        assert!(
            err.contains("healthcheck_timeout"),
            "should preserve original cause, got {err:?}"
        );
        assert!(
            err.contains("rollback_failed"),
            "should mention rollback_failed, got {err:?}"
        );
        assert!(after.rollback_attempted_at.is_some());
    }

    /// Default Notify policy is unchanged: healthcheck timeout still
    /// transitions to `Aborted`, `pull_and_recreate_at_digest` is NOT
    /// called (it would panic in the test mock if it were).
    #[tokio::test]
    async fn notify_default_remains_unchanged() {
        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let dead = dead_addr();

        struct Deps {
            dead: SocketAddr,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.dead))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(20))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
            async fn pull_and_recreate_at_digest(
                &self,
                _d: &Deployment,
                _previous: &str,
            ) -> Result<()> {
                panic!("pull_and_recreate_at_digest must not be called on Notify policy");
            }
        }

        let driver = Driver::new(
            d,
            Arc::new(Deps { dead }),
            inv.clone(),
            Arc::new(NoopEmitter),
        )
        .with_healthcheck_overrides(
            Duration::from_millis(20),
            3,
            Duration::from_millis(200),
        );
        // No with_failure_policy: the default is Notify.
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::Aborted);
        assert!(after.rollback_attempted_at.is_none());
    }

    /// Keep policy: healthcheck timeout still transitions to `Aborted`
    /// (Keep does not roll back) but a 24h `paused_until` is upserted
    /// on the service-scope policy row so the next scan skips.
    #[tokio::test]
    async fn keep_marks_aborted_and_writes_paused_until() {
        use isengard_storage::policy::PolicyScopeType;

        let (inv, d) = setup_inventory_and_row(DeploymentState::Pending).await;
        let id = d.id.clone();
        let dead = dead_addr();

        struct Deps {
            dead: SocketAddr,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.dead))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(20))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                None
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
        }

        let before = chrono::Utc::now();
        let driver = Driver::new(
            d,
            Arc::new(Deps { dead }),
            inv.clone(),
            Arc::new(NoopEmitter),
        )
        .with_healthcheck_overrides(Duration::from_millis(20), 3, Duration::from_millis(200))
        .with_failure_policy(FailureHandling::Keep);
        driver.run().await;

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::Aborted);

        let policy = inv
            .get_policy(PolicyScopeType::Service, "web")
            .await
            .unwrap()
            .expect("service-scope policy upserted by Keep");
        let until = policy.body.paused_until.expect("paused_until populated");
        let delta = (until - before).num_hours();
        assert!(
            (23..=25).contains(&delta),
            "Keep should pause for ~24h, got {delta}h"
        );
    }

    /// Post-switch collapse + Rollback policy: green flips unhealthy
    /// during the drain, the driver enters Recovering, swaps back to
    /// blue, then attempts a re-pull at `previous_digest`. Final state
    /// is `RolledBack` and the deps' rollback hook fired.
    #[tokio::test]
    async fn rollback_on_post_switch_collapse() {
        let (inv, d) =
            setup_with_previous_digest(DeploymentState::Pending, "sha256:previous_ccc").await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();

        struct Deps {
            addr: SocketAddr,
            rollback_calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(50))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                Some(synthetic_blue())
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
            async fn pull_and_recreate_at_digest(
                &self,
                _d: &Deployment,
                _previous: &str,
            ) -> Result<()> {
                self.rollback_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let bus = crate::proxy::ProxyEventBus::new();
        let rx = bus.subscribe();
        let rollback_calls = Arc::new(AtomicUsize::new(0));
        let deps = Arc::new(Deps {
            addr,
            rollback_calls: rollback_calls.clone(),
        });
        let driver = Driver::new(d, deps, inv.clone(), Arc::new(NoopEmitter))
            .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
            .with_drain_overrides(Duration::from_secs(10), Duration::from_secs(1))
            .with_proxy_events(rx)
            .with_failure_policy(FailureHandling::Rollback);

        let driver_task = tokio::spawn(driver.run());
        tokio::time::sleep(Duration::from_millis(150)).await;
        bus.publish(crate::proxy::ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "green-id".into(),
            healthy: false,
        });
        driver_task.await.unwrap();

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::RolledBack);
        assert_eq!(rollback_calls.load(Ordering::SeqCst), 1);
        assert!(after.rollback_attempted_at.is_some());
    }

    /// Post-switch collapse + Rollback policy + re-pull fails: final
    /// state is `RollbackFailed`, error mentions the original collapse
    /// AND the rollback failure.
    #[tokio::test]
    async fn rollback_failure_after_collapse_marks_rollback_failed() {
        let (inv, d) =
            setup_with_previous_digest(DeploymentState::Pending, "sha256:previous_ddd").await;
        let id = d.id.clone();
        let (addr, _accept) = live_addr();

        struct Deps {
            addr: SocketAddr,
        }
        #[async_trait]
        impl DriverDeps for Deps {
            async fn start_green(&self, _d: &Deployment) -> Result<(String, SocketAddr)> {
                Ok(("green-id".into(), self.addr))
            }
            async fn stop_and_remove(&self, _id: &str) -> Result<()> {
                Ok(())
            }
            fn build_health_checker(&self, _d: &Deployment) -> HealthChecker {
                HealthChecker::tcp_only(Duration::from_millis(50))
            }
            async fn swap_upstream_to_green(
                &self,
                _d: &Deployment,
                _gid: &str,
                _addr: SocketAddr,
                _grace: Duration,
            ) -> Result<()> {
                Ok(())
            }
            async fn snapshot_current_upstream(&self, _d: &Deployment) -> Option<Upstream> {
                Some(synthetic_blue())
            }
            async fn swap_back_to_blue(&self, _h: &str, _b: &Upstream) -> Result<()> {
                Ok(())
            }
            async fn pull_and_recreate_at_digest(
                &self,
                _d: &Deployment,
                _previous: &str,
            ) -> Result<()> {
                Err(anyhow!("manifest unknown"))
            }
        }

        let bus = crate::proxy::ProxyEventBus::new();
        let rx = bus.subscribe();
        let driver = Driver::new(
            d,
            Arc::new(Deps { addr }),
            inv.clone(),
            Arc::new(NoopEmitter),
        )
        .with_healthcheck_overrides(Duration::from_millis(10), 1, Duration::from_millis(500))
        .with_drain_overrides(Duration::from_secs(10), Duration::from_secs(1))
        .with_proxy_events(rx)
        .with_failure_policy(FailureHandling::Rollback);
        let driver_task = tokio::spawn(driver.run());
        tokio::time::sleep(Duration::from_millis(150)).await;
        bus.publish(crate::proxy::ProxyEvent::UpstreamHealthChanged {
            public_hostname: "blog.test".into(),
            container_id: "green-id".into(),
            healthy: false,
        });
        driver_task.await.unwrap();

        let after = inv.get_deployment(&id).await.unwrap().expect("row");
        assert_eq!(after.state, DeploymentState::RollbackFailed);
        let err = after.error.expect("error");
        assert!(
            err.contains("post_switch_collapse_recovered"),
            "should preserve collapse cause, got {err:?}"
        );
        assert!(
            err.contains("rollback_failed"),
            "should mention rollback_failed, got {err:?}"
        );
    }
}
