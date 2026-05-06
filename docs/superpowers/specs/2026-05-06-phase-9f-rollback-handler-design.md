# Phase 9F design: rollback failure handler

Closes #48. Couples Phase 10's blue-green machinery (atomic swap, abort, recover) with Phase 9's policy resolver (`FailureHandling` enum) so that when a deployment fails healthcheck and the resolved policy says `Rollback`, the supervisor automatically reverts to the previous digest.

Vault refs: [[Update Policies & Approval Flow]] (Rollback section), [[Blue-Green Deployment]] (existing abort/recover).

## What Phase 9F is and is not

Phase 9F is the OPT-IN failure-handler branch on the existing blue-green driver. It does NOT change the default failure path (`Notify`), does NOT touch the in-place updater path, and does NOT introduce a new state machine. It adds:

1. Two columns on `deployments`: `previous_digest TEXT` and `rollback_attempted_at TEXT`.
2. Two new `DeploymentState`s: `RolledBack` (terminal success), `RollbackFailed` (terminal failure).
3. A snapshot at deployment start: capture the blue container's digest into `previous_digest` so rollback can re-pull it even if blue has been destroyed.
4. A new branch in the supervisor's failure path that consults `policy.on_failure` and either notifies, keeps + paused_until, or rolls back.
5. Two new event kinds: `update.rolled_back`, `update.rollback_failed`.
6. UI badges on `DeploymentInProgressPanel` + `DeploymentAbortedPanel`.

## Storage shape

Migration `0022_deployments_rollback.sql` adds:

```sql
ALTER TABLE deployments ADD COLUMN previous_digest TEXT;
ALTER TABLE deployments ADD COLUMN rollback_attempted_at TEXT;
```

Plus a CHECK-constraint expansion adding `rolled_back` and `rollback_failed` to the allowed `state` set. Because SQLite cannot ALTER ... CHECK, we recreate the table (mirroring 0013's pattern) preserving every existing column.

`previous_digest` is captured at deployment-row insertion, BEFORE pulling green. The blue container is still alive at that point, so a `docker inspect blue` returns the live digest reliably. Capturing later (after green is up) would race against blue being destroyed during DestroyingBlue, defeating the purpose.

`rollback_attempted_at` is set the moment the supervisor enters the rollback branch (image pull starting), regardless of whether the rollback succeeds. The dashboard uses it to render "Rolled back at HH:MM" without parsing the error string.

## DeploymentState additions

```rust
pub enum DeploymentState {
    // existing...
    Pending, SpinningUp, Switching, Draining, DestroyingBlue,
    Recovering, Done, Aborted, Failed,
    // new (Phase 9F):
    RollingBack,    // non-terminal: re-pulling previous_digest
    RolledBack,     // terminal success: previous_digest now serving
    RollbackFailed, // terminal failure: rollback itself broke
}
```

`RollingBack` is the analog of `Recovering`. `RolledBack` and `RollbackFailed` are terminal: `is_terminal()` returns true for both. They join `Done`, `Aborted`, `Failed` in the "exclude from in-flight" predicate.

## Supervisor branch on failure

The driver's existing failure paths (`abort` in `run_inner`, `DrainOutcome::GreenUnhealthy`) currently end with a single `transition(Aborted)` or `transition(Failed)`. Phase 9F inserts a policy lookup before that transition:

```text
on healthcheck timeout / spinup fail / swap fail / green unhealthy:
    resolved = resolve_policy_for_deployment(self.deployment, ...)
    match resolved.on_failure {
        Notify => existing path: transition(Aborted), emit update.failed
        Keep   => existing path + set service-scope policy.paused_until = now + 24h
        Rollback => attempt_rollback(previous_digest):
            transition(RollingBack)
            set rollback_attempted_at = now
            re-pull previous_digest
            recreate container with that image
            on success: transition(RolledBack), emit update.rolled_back
            on failure: transition(RollbackFailed), emit update.rollback_failed
    }
```

The driver gains a new dependency injection point: a `policy_lookup: Arc<dyn PolicyLookup>` that wraps the `Inventory` + `resolve_policy` call. Production wiring uses the existing `InventoryPolicyLoader`. Tests pass a hard-coded resolver so the failure-handler branches are exercisable without wiring up the whole policy DAO.

The "re-pull" implementation reuses the existing `start_green` machinery on a new method `start_with_digest(previous_digest, ...)` that pulls the image at the recorded digest and creates a container against that exact pinned reference. This is symmetric with how green is pulled at the new digest. We deliberately do NOT swap routing back to the previous container (it's gone): we recreate the previous-digest container fresh and route to it.

## Keep + paused_until interaction

`Keep` is "leave the broken green up for forensic inspection AND don't try again for 24h." Because Phase 9F doesn't actually leave green up (the existing abort path destroys it; we'd need to change that to truly "keep"), we honour Keep as `Notify` + a service-scope `paused_until = now + 24h` write. The 24h pause is the user-visible Keep behaviour: future scans will see the active pause and skip per Phase 9b's existing `policy_decision`.

Open question resolved: should Keep insert a NEW service-scope policy row, or upsert into an existing one? Resolution: upsert. If the user already has a service-scope policy, set its `paused_until` field. If they don't, create a new row with `paused_until` only (everything else `None`, inherits from less specific scopes).

## Where previous_digest is captured

Captured at the supervisor's `handle_update_trigger` call site, BEFORE the `Driver` is spawned. The trigger already carries `blue_digest`. Phase 9F's change: when the resolved policy's `on_failure == Rollback`, copy `blue_digest` into `previous_digest` on the `InsertDeployment` payload. For `Notify` and `Keep` (and when no policy applies), leave `previous_digest` NULL: the rollback path is not eligible.

This means: enabling Rollback on an existing deployment that's already in flight is a no-op (the row was inserted with `previous_digest = NULL`). The next deployment will be eligible. Consistent with how `paused_until` works in Phase 9b.

## Failure-of-the-failure: rollback fails

Three classes:

1. **Image gone from registry.** The `previous_digest` is no longer pullable (registry GC, manual delete). `start_with_digest` returns an error; we transition to `RollbackFailed` and emit `update.rollback_failed { reason: image_unavailable }`.
2. **Resource exhaustion at recreate.** Same outcome: `RollbackFailed`, reason from the underlying error.
3. **The existing container was already destroyed in-place.** Cannot happen on the abort/healthcheck-timeout path (blue is still alive). Can happen on the `DrainOutcome::GreenUnhealthy` path (blue might have been destroyed by mid-drain, though Phase 10c's drain-buffer keeps it alive through the drain window). If blue is gone but `previous_digest` is recorded, we still try to start a fresh container at that digest. Same code path.

`RollbackFailed` requires operator action: the dashboard surfaces a red badge with a Retry button that re-queues the rollback (next supervisor scan).

## Events

```text
update.rolled_back     summary="rolled back <service> to <previous_digest_short>"
                       metadata.deployment = full row
                       metadata.previous_digest = <full sha256>

update.rollback_failed summary="rollback failed for <service>: <reason>"
                       metadata.deployment = full row
                       metadata.error = <stringified error>
                       metadata.previous_digest = <full sha256>
```

`deployment.rolled_back` and `deployment.rollback_failed` also fire as the canonical state-transition events (one per `transition(state)` call), matching the existing convention for every other deployment state.

## UI surfaces

**`DeploymentInProgressPanel.vue`**: the linear STEPS array gets an optional "rolling back" step appended when `state === 'rolling_back'`. The header dot turns warn-orange. State pill shows "rolling back".

**`DeploymentAbortedPanel.vue`**: two new branches.
- `state === 'rolled_back'`: green-tinted panel, badge "Rolled back to previous digest", body shows the `previous_digest` short form. No Retry button (the rollback succeeded; there's nothing to retry).
- `state === 'rollback_failed'`: red panel, badge "Rollback failed", body shows the original error PLUS the rollback error. Retry button re-queues a force-update.

`DeploymentDto` (in `useDeployments.ts`) gains `previous_digest?: string` and `rollback_attempted_at?: string` as optional strings.

## Why this matters

Without Phase 9F, "blue-green deployment" is half a story: failure aborts and leaves the operator to manually remediate. With 9F, the most demanded operator config (auto rollback on failure) works end-to-end. It's also the smallest Plan-A-mentioned feature still missing from the policy story; closing #48 closes the v0.2 trust kit's last update-policy gap.

## Implementation phasing

Single PR, six tasks, one commit each:

1. Storage migration 0022 + DAO setters + state-string round-trip + 4 unit tests.
2. Supervisor: capture `previous_digest` when `on_failure == Rollback`. Inject `PolicyLookup`. 2 supervisor tests.
3. Driver: add `attempt_rollback`, wire it into spinup-fail / healthcheck-timeout / swap-fail paths. 4 driver tests.
4. Driver: wire it into `DrainOutcome::GreenUnhealthy` (post-switch collapse + Rollback). 2 driver tests. End-to-end real-Docker test.
5. UI: extend Vue panels + DTO. 2 component tests.
6. Wrap-up: design page status, release notes, final gate sweep, PR.
