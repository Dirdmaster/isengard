# Phase 9F plan: rollback failure handler

Spec: `docs/superpowers/specs/2026-05-06-phase-9f-rollback-handler-design.md`. Closes #48.

Six tasks, one commit each. All work on branch `feat/phase-9f` off `next` HEAD `1ab6571` (Phase 10c).

## T1: storage migration 0022 + DAO

Files:
- `crates/isengard-storage/migrations/0022_deployments_rollback.sql` (new): recreate table to add `rolled_back` + `rollback_failed` to the state CHECK, add columns `previous_digest TEXT` and `rollback_attempted_at TEXT`.
- `crates/isengard-storage/src/deployment.rs`:
  - Extend `DeploymentState` with `RollingBack`, `RolledBack`, `RollbackFailed`. Update `as_str` + `FromStr` + `is_terminal` (the latter two terminals join Done/Aborted/Failed).
  - `Deployment` gains `previous_digest: Option<String>` + `rollback_attempted_at: Option<DateTime<Utc>>`.
  - `InsertDeployment` gains `previous_digest: Option<String>`.
  - New setter `set_deployment_rollback_attempted(id, at)`.
  - New setter `set_deployment_previous_digest(id, digest)` (used by tests + by the supervisor when patching mid-flight, though primary write happens via insert).
  - Update INSERT, SELECT, and `upsert_deployment_from_remote` to carry the new columns.
  - Update `mark_orphan_deployments_failed` and `list_in_flight_*` predicates to exclude the new terminal states.

Tests added to `crates/isengard-storage/src/deployment.rs#tests`:
1. `insert_with_previous_digest_round_trips`: insert with `previous_digest = Some("sha256:old")`, read back, assert.
2. `set_rollback_attempted_persists_timestamp`: insert, call setter, assert RFC3339 round-trip.
3. `state_round_trips_for_new_states`: parse + as_str for `RollingBack`, `RolledBack`, `RollbackFailed`.
4. `terminal_includes_rolled_back_and_rollback_failed`: assert `is_terminal` covers both new terminals; `RollingBack` is not terminal.
5. `list_in_flight_excludes_rollback_terminals`: insert one `RolledBack` + one `RollbackFailed` + one `SpinningUp`, assert `list_in_flight_deployments` returns only the SpinningUp.

Commit: `feat(storage): migration 0022 rollback columns + DAO setters (#48)`

## T2: supervisor captures previous_digest + injects PolicyLookup

Files:
- `crates/isengard-core/src/policy_loader.rs`: extend `PolicyLoader` trait with `async fn resolve_for_service(&self, host_id, fleet, stack, service) -> Option<ResolvedPolicy>`. Default impl uses `list()` + the existing `resolve_policy` helper. (Backwards compat: existing `list` impls keep working; `resolve_for_service` gets a default body.)
- `crates/isengard-agent/src/deployment/mod.rs`:
  - `DeploymentSupervisor::new` gains an optional `policy_loader: Option<Arc<dyn PolicyLoader>>` field. Constructed via a new builder method `with_policy_loader` so existing call sites compile unchanged.
  - In `handle_update_trigger`, after the dedupe + classification but before the `insert_deployment` call: if `policy_loader.is_some()` and the eligibility decision is `BlueGreen`, resolve the policy. If `on_failure == Rollback`, set `previous_digest = Some(trigger.blue_digest.clone())` on the `InsertDeployment` payload.
  - Pipe the resolved policy through to the `Driver::new` call (via a new builder method `with_failure_policy(FailureHandling)` so the driver knows what branch to take).
- `crates/isengard-agent/src/lib.rs` (or wherever the supervisor is wired): pass `Arc::new(InventoryPolicyLoader::new(...))` to `with_policy_loader`.

Tests added to `crates/isengard-agent/src/deployment/mod.rs#supervisor_tests`:
1. `rollback_policy_captures_previous_digest`: install a service-scope policy with `on_failure = Rollback`, dispatch a BlueGreen trigger, assert the inserted row has `previous_digest = Some(blue_digest)`.
2. `notify_policy_leaves_previous_digest_null`: same but with `on_failure = Notify` (the default), assert `previous_digest = None`.

Commit: `feat(agent): supervisor captures previous_digest under Rollback policy (#48)`

## T3: driver `attempt_rollback` for spinup/healthcheck/swap failures

Files:
- `crates/isengard-agent/src/deployment/driver.rs`:
  - Extend `DriverDeps` with `async fn pull_and_recreate_at_digest(&self, deployment, previous_digest) -> Result<()>`.
  - `Driver` gains `failure_handling: FailureHandling` (default Notify) + builder `with_failure_policy`.
  - New helper `async fn attempt_rollback(&mut self, original_error: String) -> ()`:
    - if `self.deployment.previous_digest.is_none()` -> emit `update.failed` (no rollback eligible) and call existing abort path.
    - else: set `rollback_attempted_at = now`, transition `RollingBack`, call `pull_and_recreate_at_digest`. On success: transition `RolledBack`, emit `update.rolled_back`. On failure: transition `RollbackFailed`, emit `update.rollback_failed`.
  - In the `abort` helper: if `failure_handling == Rollback`, take the rollback branch instead of `transition(Aborted)`. If `failure_handling == Keep`, after transition Aborted, fire-and-forget upsert of a service-scope policy with `paused_until = now + 24h` (best-effort: a failure here just logs).
- `crates/isengard-storage/src/policy.rs`: add helper `set_service_paused_until(host_id, fleet, stack, service, until)` that upserts a service-scope policy row with `paused_until` filled and other fields preserved if the row exists.

Tests added to `crates/isengard-agent/src/deployment/driver.rs#tests`:
1. `rollback_on_healthcheck_timeout_succeeds`: deps with `pull_and_recreate_at_digest = Ok(())`, deployment row with `previous_digest = Some(...)`, `failure_handling = Rollback`. Drive a healthcheck-timeout flow. Assert final state is `RolledBack`, `rollback_attempted_at` is populated, `pull_and_recreate_at_digest` was called once.
2. `rollback_on_healthcheck_timeout_fails_when_image_gone`: same but `pull_and_recreate_at_digest` returns Err (image not found). Assert final state is `RollbackFailed`, error contains the original cause.
3. `notify_default_remains_unchanged`: `failure_handling = Notify`, healthcheck timeout. Assert final state is `Aborted` (existing behaviour). `pull_and_recreate_at_digest` was NOT called.
4. `keep_marks_aborted_and_writes_paused_until`: `failure_handling = Keep`, capture a fake policy-store mock, healthcheck timeout. Assert state is `Aborted` AND the mock's `set_service_paused_until` was called with a future timestamp.

Commit: `feat(agent): driver attempt_rollback on healthcheck/spinup/swap fail (#48)`

## T4: rollback for post-switch collapse + e2e test

Files:
- `crates/isengard-agent/src/deployment/driver.rs`: extend the `DrainOutcome::GreenUnhealthy` arm to consult `failure_handling`. Same Rollback branch as T3.
- `crates/isengard-agent/tests/deployment_blue_green_rollback.rs` (new): real-Docker e2e mirroring `deployment_blue_green_aborts_on_healthcheck`. Setup: blue running nginx, healthcheck path returning 404 (so green never goes healthy), policy with `on_failure = Rollback`, `previous_digest` populated. Assert the deployment ends in `RolledBack`, the previous-digest container is up, blue's slot is reclaimed.

Tests added to `driver.rs#tests`:
5. `rollback_on_post_switch_collapse`: drive into the drain window, fire `UpstreamHealthChanged { healthy: false }` for green, with `failure_handling = Rollback` + previous_digest set. Assert final state is `RolledBack` and `pull_and_recreate_at_digest` was called.
6. `rollback_failure_after_collapse_marks_rollback_failed`: same but `pull_and_recreate_at_digest` returns Err. Assert `RollbackFailed`.

Commit: `feat(agent): rollback on post-switch collapse + e2e (#48)`

## T5: UI panels + DTO

Files:
- `crates/isengard-plugins/dashboard/web/composables/useDeployments.ts`: add `previous_digest?: string`, `rollback_attempted_at?: string` to `DeploymentDto`.
- `crates/isengard-plugins/dashboard/web/components/DeploymentInProgressPanel.vue`: append a "rolling back" step when `state === 'rolling_back'`. Header dot turns warn when state matches. State pill renders "rolling back".
- `crates/isengard-plugins/dashboard/web/components/DeploymentAbortedPanel.vue`:
  - Branch on `state === 'rolled_back'`: green-tinted panel, badge "Rolled back to previous digest" with the short previous_digest. No Retry.
  - Branch on `state === 'rollback_failed'`: red panel, badge "Rollback failed". Show `error`. Retry button posts to existing force-update endpoint.
- `crates/isengard-plugins/dashboard/src/deployments.rs`: extend the deployment serializer to include the two new columns.

Tests added:
- `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs` (extend if exists, else new): GET `/api/v1/deployments/<id>` returns `previous_digest` and `rollback_attempted_at` when set.

Commit: `feat(dashboard): rolled_back + rollback_failed UI states (#48)`

## T6: wrap-up

Files:
- `design/pages/settings-policies.md`: move Rollback handler from Deferred to Shipped.
- `docs/RELEASE_NOTES_PHASE_9F.md`: operator-facing notes including "set on_failure = Rollback" example.

Verification: full `cargo test --workspace` clean (excluding the e2e `--ignored` test, which gets a separate manual run-line in the release notes).

Commit: `chore: phase 9f wrap-up (release notes + design status) (#48)`

Push branch + open PR vs `next`. Body: "Closes #48". Title: `feat: phase 9f (rollback handler)`.
