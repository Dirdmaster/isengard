# Phase 9F: Rollback Failure Handler

Builds on Phase 9a-9d (policy foundation), Phase 9e (approval flow), and Phase 10 (blue-green deployment). With this release, the `on_failure: Rollback` field on a policy is no longer a configurable-but-inert setting: when a Rollback-policy deployment fails healthcheck or collapses post-switch, the supervisor automatically reverts to the previous digest.

Closes issue #48.

## What's new

- New deployment states: `rolling_back` (non-terminal), `rolled_back` (terminal success), `rollback_failed` (terminal failure).
- Two new columns on `deployments`: `previous_digest TEXT` (snapshot of the blue digest taken at deployment start when on_failure = Rollback) and `rollback_attempted_at TEXT` (set when the supervisor enters the rollback branch).
- The `DeploymentSupervisor` consults a `PolicyLoader` per trigger. When `on_failure == Rollback`, `previous_digest` is seeded from `blue_digest` so the driver can re-pull it later.
- New event kinds: `update.rolled_back`, `update.rollback_failed`. Both carry the full deployment row in event metadata.
- `Keep` policy is now respected: a deployment failure under `Keep` still aborts as before, but additionally upserts `paused_until = now + 24h` on the service-scope policy row so the next updater scan skips the service for a day.
- Dashboard: `DeploymentInProgressPanel` shows a "Rolling back" row when state matches; `DeploymentAbortedPanel` renders distinct success/failure badges for rolled_back / rollback_failed and hides Retry on rolled_back (nothing to retry).

## How rollback works

1. Updater detects a digest drift.
2. Supervisor classifies the trigger as blue-green eligible.
3. Supervisor consults the policy loader. The resolved `on_failure` is one of `Notify` (default), `Keep`, or `Rollback`.
4. If `Rollback`: the supervisor inserts the deployment row with `previous_digest = blue_digest`, then spawns the driver with `with_failure_policy(Rollback)`.
5. Driver runs the standard blue-green flow.
6. If green fails (spinup, healthcheck timeout, swap, post-switch collapse), the driver:
   - Cleans up green.
   - Stamps `rollback_attempted_at = now`.
   - Transitions `rolling_back`.
   - Calls `pull_and_recreate_at_digest(deployment, previous_digest)`. Production impl pulls `<repo>@<previous_digest>` and creates a fresh container against blue's prior config.
   - On success: transitions `rolled_back`, emits `update.rolled_back`.
   - On failure: transitions `rollback_failed`, emits `update.rollback_failed` with the combined original + rollback errors.

## Decisions captured

- **Where `previous_digest` is captured**: at the supervisor's `handle_update_trigger` call site, BEFORE the Driver is spawned. The trigger already carries `blue_digest`. Capturing later would race against blue being destroyed by mid-deployment cleanup. Pre-existing deployments inserted before 9F have `previous_digest = NULL` and stay rollback-ineligible by design (no migration backfill: enabling Rollback applies to NEW deployments).
- **Keep + `paused_until`**: Keep upserts a service-scope policy row with `paused_until = now + 24h`, preserving any existing fields. The next scan's standard 9b `Skip(Paused)` branch handles the pause; no new code path needed in the updater. Future scans see the active pause and emit `update.policy_skipped` until 24h elapses.
- **Rollback when blue is gone**: the production `pull_and_recreate_at_digest` falls back to "RollbackFailed" if blue's container has been destroyed mid-deploy and we can't derive the bare repo name. The recorded digest is still preserved on the row so an operator can manually re-pull.

## Setup steps

None. Existing deployments without a Rollback policy continue to work unchanged. To opt a service into Rollback:

1. Open Settings -> Policies.
2. Pick a scope (fleet / stack / service / container).
3. Toggle "Override at this level" next to "On failure".
4. Pick `rollback`.
5. Save.

The next deployment for any service that resolves to this policy will be rollback-eligible.

## Breaking changes

None. Migration `0022_deployments_rollback.sql` is a recreate-table-add-columns migration that preserves every existing row. New columns default to NULL, which means rollback-ineligible (matches pre-9F behaviour).

## Backwards compat verified

- `cargo test --workspace` clean
- All 5 storage DAO tests pass
- All 32 driver + supervisor unit tests pass
- The e2e test (`crates/isengard-agent/tests/deployment_blue_green_rollback.rs`) is gated `--ignored` and runs against a real dockerd: `cargo test -p isengard-agent --test deployment_blue_green_rollback -- --ignored --nocapture`.

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9F.1 | Operator-triggered manual rollback button on the deployment detail page (today rollback is purely policy-driven). |
| 9F.2 | "Rollback to N versions ago" support: today only the immediately-previous digest is captured. |
| 9g | Discord interactive callbacks (independent of this release). |
| 9i | `Minor` strategy semver-aware tag bumping. |

## Notes

- An operator-approved force-update applies regardless of `paused_until` (the 24h pause from Keep is not absolute; explicit human action overrides). Same precedence as Phase 9d's maintenance window.
- The rollback path does not currently swap the proxy upstream back to the rolled-back container. The recovery `swap_back_to_blue` from Phase 10f handles routing during the drain window; once routing is restored to blue, the rolled-back container takes over blue's slot via the existing container-name semantics (Pingora picks up the new container at the same port). This is fine for typical web services; future work may want to make the proxy state explicit during rollback.
