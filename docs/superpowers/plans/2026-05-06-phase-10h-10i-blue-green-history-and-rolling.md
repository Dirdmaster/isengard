# Phase 10 Plan C (10h+10i): Blue-Green History + Multi-Host Rolling

Implements [[2026-05-06-phase-10h-10i-blue-green-history-and-rolling-design]] via subagent-driven workflow.

Branch: `feat/phase-10c`
Worktree: `~/Projects/isengard/.worktrees/phase-10c`
Base: `next` at HEAD with Plan B merged (or imminent — handle either)
Migration slot: `0018`

Implementer model: **Opus** for every task.

## Standing self-review (every task)

1. cargo build --workspace
2. cargo test --workspace
3. cargo clippy --workspace --all-targets -- -D warnings
4. cargo fmt --check
5. Em dash (U+2014) and en dash (U+2013) scan over changed files: zero hits
6. bun run build for dashboard tasks
7. Cite added/modified files

## Tasks

### T1: Storage — deployment_groups + parallelism

Files:
- `crates/isengard-storage/migrations/0018_deployment_groups.sql` (new) per spec
- `crates/isengard-storage/src/deployment_group.rs` (new): types + DAO
- `crates/isengard-storage/src/deployment.rs` (extend): `set_deployment_group(deployment_id, group_id)`, `list_deployments_by_group(group_id)`
- `crates/isengard-storage/src/stack.rs` (or wherever stack metadata lives, search for it): `set_stack_parallelism`, `get_stack_parallelism`
- `crates/isengard-storage/src/lib.rs`: re-exports
- `crates/isengard-storage/tests/deployment_group_dao.rs` (new): 12+ tests covering insert/get/list/state transitions/group linkage/parallelism upsert

Commit: `feat(storage): deployment_groups + stack parallelism (T1 phase 10c)`

### T2: Controller orchestrator (10i)

Files:
- `crates/isengard-controller/src/stack_deploy_orchestrator.rs` (new): subscribes to update.detected events at the stack level (or accepts dispatched group requests via API). Implements wave plan + event subscription. State machine with `tokio::select!` over (event_bus, periodic_reconcile_tick, abort_signal).
- `crates/isengard-controller/src/lib.rs`: spawn orchestrator at startup
- `crates/isengard-controller/tests/orchestrator_e2e.rs` (new): 6+ tests: 3-host parallelism=1 rolls 1 then 1 then 1; parallelism=all dispatches all at once; abort on wave 1 fail; single-host bypasses; group state transitions; reconciliation when an event is missed

Approach: keep this module isolated from the agent's local deployment supervisor. Agents don't change behavior — they receive `apply_update` HostActions as before. The orchestrator is a controller-side coordinator that DECIDES WHEN to dispatch each wave.

Commit: `feat(controller): stack deploy orchestrator with parallelism waves (T2 phase 10c)`

### T3: REST endpoints

Files:
- `crates/isengard-plugins/dashboard/src/deployment_groups.rs` (new): handlers + router
- `crates/isengard-plugins/dashboard/src/deployments.rs` (extend): add `group_id` filter param
- `crates/isengard-plugins/dashboard/src/lib.rs`: mount new router
- `crates/isengard-plugins/dashboard/tests/deployment_group_endpoints.rs` (new): 10+ tests

Endpoints per spec.

Commit: `feat(dashboard): /api/v1/deployment-groups + parallelism setting (T3 phase 10c)`

### T4: UI — History tab polish (10h)

Files:
- `crates/isengard-plugins/dashboard/web/components/stacks/StackHistoryTab.vue` (rewrite): filter chips, time range, row expand showing timeline
- `crates/isengard-plugins/dashboard/web/composables/useDeploymentEvents.ts` (new): fetch events for a deployment_id
- Show group icon + tooltip when group_id is present

`bun run build` green; visual smoke.

Commit: `feat(dashboard): history tab filters + row expand + group indicator (T4 phase 10c)`

### T5: UI — Group progress + parallelism setting (10i)

Files:
- `crates/isengard-plugins/dashboard/web/components/stacks/DeploymentGroupPanel.vue` (new): progress bar + per-host strip + abort button
- `crates/isengard-plugins/dashboard/web/composables/useDeploymentGroups.ts` (new): SWR + websocket-style live (existing pattern in useDeployments.ts)
- `crates/isengard-plugins/dashboard/web/components/stacks/[id].vue` (extend): mount DeploymentGroupPanel above the existing in-progress panel when a group is rolling
- `crates/isengard-plugins/dashboard/web/components/DeploymentsSettings.vue` (extend): add per-stack parallelism dropdown

Commit: `feat(dashboard): deployment group panel + parallelism dropdown (T5 phase 10c)`

### T6: Wrap-up + design status + release notes + PR

Files:
- `design/pages/stack-detail.md`: add to Implementation status — group panel, history filters
- `design/pages/settings-deployments.md`: add parallelism dropdown
- `docs/RELEASE_NOTES_PHASE_10C.md` (new): operator-facing
- Final gate sweep
- Push + open PR vs `next`. Body summarizes shipped + deferred (sticky session, db migration coupling, pre-deploy hooks).

Commit: `chore: phase 10c wrap-up (design status + release notes)`

## Execution order

T1 first. Then T2 + T3 in parallel (different crates). Then T4 + T5 in parallel (UI; T5 needs T3's group endpoint). T6 last.

## Final gates

`cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo deny check && (cd crates/isengard-plugins/dashboard/web && bun run build)`
