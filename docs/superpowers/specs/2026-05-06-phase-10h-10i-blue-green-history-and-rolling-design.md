# Phase 10 Plan C (10h+10i): Blue-Green History + Multi-Host Rolling

Closes the blue-green story shipped in Plan A (10a-10d, core) + Plan B (10e-10g, UI). Plan C polishes the deployment history surface and adds multi-host orchestration.

Scope:
- **10h**: Deployment history table is shipped (basic). Polish: row expansion showing event timeline, filter chips (state / service / time range), postmortem affordances.
- **10i**: Multi-host rolling parallelism. Stack-level `deployment.parallelism` setting (1 | N | all); controller orchestrates the per-host sequence; UI surfaces group progress.

Out of scope:
- Sticky-session / WebSocket grace tuning (per-rule connection_lifetime_strategy is documented in design but defer)
- Database migration coupling (expand-contract pattern)
- Pre-deploy hooks (couples with Phase 12 Hooks)
- Resource pre-flight (defer; agent-side work)

## Storage

### Migration 0018

```sql
-- Stack-level parallelism setting.
ALTER TABLE stacks ADD COLUMN deployment_parallelism TEXT;
-- Allowed values: NULL (defaults to 1), '1', '2'..'N', 'all'.
-- Stored as TEXT to preserve 'all' sentinel without loss.

-- Multi-host deployment grouping.
CREATE TABLE deployment_groups (
    id              TEXT PRIMARY KEY,           -- ULID
    stack_id        INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name    TEXT NOT NULL,
    parallelism     TEXT NOT NULL,              -- snapshot at start
    state           TEXT NOT NULL,              -- pending | rolling | done | aborted | failed
    target_hosts    TEXT NOT NULL,              -- JSON array of host_id hex
    started_at      TEXT NOT NULL,
    finished_at     TEXT,
    error           TEXT
);

CREATE INDEX idx_deployment_groups_state ON deployment_groups(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

-- Per-deployment group reference.
ALTER TABLE deployments ADD COLUMN group_id TEXT REFERENCES deployment_groups(id);
```

State transitions for `deployment_groups`: `pending -> rolling -> (done | aborted | failed)`.

### DAO additions

On `Inventory`:
- `set_stack_parallelism(stack_id, parallelism: Option<&str>)` — upsert.
- `get_stack_parallelism(stack_id) -> Option<String>`.
- `insert_deployment_group(InsertDeploymentGroup) -> DeploymentGroup`.
- `get_deployment_group(group_id) -> Option<DeploymentGroup>`.
- `list_deployment_groups(stack_id, limit) -> Vec<DeploymentGroup>` ordered started_at DESC.
- `update_deployment_group_state(group_id, state, error?) -> ()`.
- `list_deployments_by_group(group_id) -> Vec<Deployment>` ordered created_at ASC.
- `set_deployment_group(deployment_id, group_id)` — link an existing deployment row to a group.

12+ unit tests.

## Controller orchestration (10i)

In `isengard-controller`'s deployment supervisor (or a new `stack_deploy_orchestrator` module):

When a stack triggers an update across multiple hosts (image scan picks up a new digest on N hosts simultaneously, OR a force-update is dispatched stack-wide):

1. Read `stack.deployment_parallelism`. NULL or absent -> `1` (rolling, one at a time).
2. Insert a `deployment_groups` row with state=pending, target_hosts = the affected host_ids, parallelism = the resolved value.
3. Compute the wave plan: split target_hosts into batches of size `parallelism` (or all in one batch if 'all'). Order is alphabetical by host_id_hex (deterministic).
4. Dispatch wave 0: insert N `deployments` rows with group_id set, dispatch `apply_update` HostAction to each batch's hosts.
5. Subscribe to `deployment.completed` and `deployment.aborted`/`deployment.failed` events. When all deployments in wave K are terminal:
   - If any aborted/failed and `on_failure=Rollback`: stop rolling forward, mark group=aborted, do NOT dispatch wave K+1. Surface error.
   - Else: dispatch wave K+1.
6. When all waves done with all deployments succeeded: mark group=done.

The orchestrator is a separate task spawned from controller startup. State machine driven by event bus subscription.

For single-host deployments (typical homelab): NO orchestrator overhead — the existing per-host deployment supervisor handles it directly. The orchestrator only kicks in when target_hosts.len() > 1.

## REST

| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/deployment-groups?stack_id=&state=&limit=` | List groups for a stack (or globally). |
| GET | `/api/v1/deployment-groups/:id` | Single group with embedded deployments[]. |
| POST | `/api/v1/stacks/:id/deployment-parallelism` body `{ parallelism: "1"|"N"|"all"|null }` | Set parallelism. |
| GET | (extend `/api/v1/deployments`) | Add `group_id` filter param. |

10+ tests.

## UI

### Stack detail History tab polish (10h)

Existing `StackHistoryTab.vue`:
- Add filter chips: state (all / done / failed / aborted / in-flight), strategy (blue-green / in-place), service.
- Add time-range chip (1h / 24h / 7d / all).
- Add row expand: clicking a row reveals an inline timeline of events for that deployment_id (fetch from `/api/v1/events?deployment_id=`). Show transitions with timestamps + reason text.
- For grouped deployments (group_id set), show a small group-icon + tooltip "Part of multi-host deploy (1 of 3)".
- Empty state stays as-is.

### Stack detail group progress (10i)

When a deployment_group is in flight:
- New `<DeploymentGroupPanel />` mounted on Stack detail above the existing DeploymentInProgressPanel.
- Shows: progress bar (X of N hosts done), per-host strip (chip per host with state), strategy (rolling/all), abort button.
- Collapses to a one-line success badge on done.

### Settings to Deployments parallelism dropdown (10i)

Existing Settings -> Deployments tab (Phase 10g) shows per-service strategy override. Extend with a per-stack parallelism dropdown:
- Header: "Multi-host deploys: <stack_name>"
- Dropdown: rolling (1) / parallel 2 / parallel 3 / all (use 1, 2, 3, all options; the 'N' option could be a custom number input, but for v1 keep the dropdown to 1/2/3/all).
- Help: "Controls how many hosts deploy in lockstep when the same stack runs on multiple hosts. Default: rolling (1 at a time)."

## Acceptance

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo deny check` clean
- [ ] `bun run build` clean
- [ ] Migration 0018 applies cleanly
- [ ] Multi-host deploy with parallelism=1 visibly rolls one host at a time (integration test)
- [ ] Single-host deploy bypasses orchestrator (no group row created)
- [ ] History tab row expand shows event timeline
- [ ] No em dashes in any new file

## Risks

- **Event subscription correctness**: orchestrator depends on receiving terminal events for all dispatched deployments. Network partitions might delay. Reconcile via periodic check: if a wave's deployments have been in-flight for > N minutes, query their state directly from storage to break ties.
- **Concurrent stack updates**: two image changes on different services of the same stack creating two groups simultaneously is fine — they're independent rows and per-service state machines.
- **History tab perf**: a stack with 1000 historical deployments might lag. Add `limit=50` default + pagination later if profiling shows hot.

## References

- Vault: [[Blue-Green Deployment]] (full design)
- Plan A spec: `2026-05-04-phase-10a-10d-blue-green-core-design.md`
- Plan B spec: `2026-05-04-phase-10e-10g-blue-green-ui-design.md`
- Existing supervisor: `crates/isengard-agent/src/deployment/supervisor.rs`
- Existing history tab: `crates/isengard-plugins/dashboard/web/components/stacks/StackHistoryTab.vue`
