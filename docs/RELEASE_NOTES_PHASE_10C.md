# Phase 10c: Blue-Green History + Multi-Host Rolling

**Closes Plan C of the Phase 10 blue-green track.** Builds on Plan A (10a-10d, core driver) and Plan B (10e-10g, UI). Plan C polishes the deployment history surface and adds multi-host orchestration.

This release is operator-facing: nothing changes for single-host homelab installs. Multi-host fleets gain a coordinated rolling-deploy mode and a richer history view.

## What's new

### Multi-host rolling deploys (10i)

Stack-wide updates that fan out to more than one host now go through a controller-side orchestrator that decides how many hosts deploy in lockstep.

- New per-stack setting: `deployment_parallelism`. Values: `1` (default, rolling, one host at a time), `2`, `3`, ..., `N`, or `all`.
- New `deployment_groups` row tracks each multi-host deploy as one logical unit. Single-host deploys never produce a group row and bypass the orchestrator entirely.
- Wave plan is deterministic: hosts are sorted by id and chunked by parallelism. If wave K has any failed/aborted deployments and `on_failure=Rollback` (default), the orchestrator stops rolling forward and marks the group `aborted`.
- Reconciliation: a periodic tick re-queries storage for the current wave's deployment states, so a missed event never strands the group.

### History tab polish (10h)

The stack detail History tab now ships filter chips, a row-expand timeline, and a multi-host indicator.

- Filter chips: state (`all` / `done` / `failed` / `aborted` / `in-flight`), strategy (`all` / `blue-green` / `in-place`), service (auto-populated from history), time range (`1h` / `24h` / `7d` / `all`).
- Row expand: clicking a row reveals the per-deployment event timeline, fetched via the new `/api/v1/events?deployment_id=` filter.
- Group indicator: rows that belong to a multi-host rolling deploy show a `group` chip with a tooltip pointing at the group id. Click-through to the active group panel is handled on stack detail.

### In-flight group panel

When a deployment group is rolling, the stack detail page shows a new `<DeploymentGroupPanel />` above the existing in-progress panel:

- Progress bar: `X of N hosts done`.
- Per-host chip strip: each host shows its current deployment state (`spinning_up` / `switching` / `done` / `failed` / etc).
- `Abort group` button: marks the group `aborted` immediately; subsequent waves are skipped.

### Settings: per-stack parallelism dropdown

The Deployments settings tab grew a per-stack dropdown next to each stack header (`Rolling (1)` / `Parallel 2` / `Parallel 3` / `All at once`). Persists to `/api/v1/stacks/:id/deployment-parallelism`.

## REST surface

| Method | Path | Notes |
| ------ | ---- | ----- |
| GET    | `/api/v1/deployment-groups?stack_id=&state=&limit=` | List groups (per stack or globally). `state` accepts `pending` / `rolling` / `done` / `aborted` / `failed` / `active`. |
| GET    | `/api/v1/deployment-groups/:id` | Single group with embedded `deployments[]`. |
| DELETE | `/api/v1/deployment-groups/:id` | Mark a stuck group `aborted`. Idempotent (returns 200 if already terminal). |
| GET    | `/api/v1/stacks/:id/deployment-parallelism` | Read the persisted parallelism value. `null` means default rolling. |
| POST   | `/api/v1/stacks/:id/deployment-parallelism` body `{"parallelism":"1"|"2"|"all"|null}` | Set or clear the parallelism. |
| GET    | `/api/v1/deployments?group_id=` | Phase 10c filter. Returns every deployment in the group regardless of state. |
| GET    | `/api/v1/events?deployment_id=` | Phase 10c filter. Returns events whose `metadata.deployment.id` matches; backed by widened journal scan up to 5k rows. |

## Storage

Migration `0018_deployment_groups.sql`:

- `stacks.deployment_parallelism` (TEXT, nullable). NULL = default rolling.
- New `deployment_groups` table with state machine `pending -> rolling -> (done | aborted | failed)`.
- `deployments.group_id` (TEXT, FK on `deployment_groups`). NULL for single-host deploys.

## Operator notes

- Existing single-host fleets see no behaviour change. The orchestrator only kicks in when `target_hosts.len() > 1`.
- Default parallelism is rolling (1 at a time). Setting `all` is the right call only when your service tolerates simultaneous restarts on every host (rare for HTTP services with sticky upstreams).
- `parallelism=2` and `parallelism=3` are the practical sweet spots for fleets of 4+ hosts.
- Aborted groups stay in storage; the abort never destroys per-host deployment rows.

## Deferred (not in this release)

- Sticky-session / WebSocket grace tuning per routing rule. Documented in the design but waits for a real workload that needs it.
- Database expand-contract migration coupling.
- Pre-deploy hooks (waits for Phase 12 Hooks).
- Resource pre-flight checks on the agent.

## References

- Spec: `docs/superpowers/specs/2026-05-06-phase-10h-10i-blue-green-history-and-rolling-design.md`
- Plan: `docs/superpowers/plans/2026-05-06-phase-10h-10i-blue-green-history-and-rolling.md`
- Issue: `Dirdmaster/isengard#50`
