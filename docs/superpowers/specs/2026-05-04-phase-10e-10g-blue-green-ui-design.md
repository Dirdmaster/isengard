# Phase 10 Plan B — Blue-Green UI + Abort + Settings Override (10e-10g)

**Status:** spec
**Phase:** 10e-10g
**Date:** 2026-05-04
**Stacked on:** PR #21 (`feat/blue-green-core`, Plan A)
**Vault design:** `1 Projects/Isengard/Blue-Green Deployment.md`

## What this PR builds

Surfaces Plan A's blue-green deployment driver to users and adds the missing post-switch failure recovery:

- **10e** — In-flight progress panel on the Stack detail page. Real-time updates via WebSocket.
- **10f** — Abort button + post-switch collapse handling. Driver gains a `Recovering` state; if the green container becomes unhealthy during the drain window, the driver swaps traffic back to the still-alive blue.
- **10g** — Settings tab for per-service deployment strategy override (`auto` / `blue-green` / `in-place`). Stored on the `services` table.

What this is NOT: deployment history tab (Plan C 10h), multi-host rolling parallelism (10i), per-rule healthcheck-spec config UI, retry-with-overrides flow.

## Goals

1. Make in-flight deployments visible. Real-time, no manual refresh.
2. Make aborts safe and fast. Click → driver responds within sub-second; blue keeps serving regardless.
3. Make post-switch collapse recoverable. If green dies between swap and blue-destruction, swap traffic back automatically. The user sees a `Failed` deployment with a clear reason; service availability is preserved.
4. Let users choose the strategy per service. Auto-detection is the default; explicit override per service via UI; container label override still wins (matches Plan A).

## Non-goals (deferred)

| Deferred | Where it lands |
|---|---|
| Deployment history tab (`/stacks/:id/deployments`) | Plan C (10h) |
| Multi-host rolling parallelism | Plan C (10i) |
| Per-rule healthcheck spec UI (`success_threshold`, `deadline_secs`) | Future PR |
| Retry button with adjusted parameters | Future PR (button shows up but is wired to "trigger updater cycle" v1) |
| View green logs from aborted panel | Phase 13 (logs streaming) |
| Adjust healthcheck from aborted panel | Future, ties to per-rule spec UI |
| Confirmation modal on abort | Skipped — abort is safe (blue keeps serving), one-click ships faster |
| Aggregating multiple in-flight deployments per stack | v1 shows ONLY the most-recently-started active deployment with a non-interactive `+N more` badge if there are others. Cycling between them deferred. |
| Eligibility annotations in settings ("✓ blue-green eligible" / "⚠ no healthcheck") | Deferred. Eligibility data isn't on the `services` table — would require a per-request inspect or a new cached `service_meta`. v1 shows just the override radio; user sees actual classification in the events log when the next deployment fires. |
| "Last 3 attempts" structured cards on aborted panel | Plan A's Driver writes the failure as a string error (e.g. `"healthcheck_timeout: 12 attempts logged"`). v1 displays the raw string under "Reason:" — no structured attempts UI. Restructuring Plan A's error metadata is deferred. |
| Disabled-stub buttons ("View green logs", "Adjust healthcheck") on aborted panel | Don't render them at all in v1. Hiding > showing-disabled. |

## Architecture

### Data flow: agent → controller → dashboard

The Plan A `Deployment` row lives in the agent's local SQLite. The controller never had visibility. Plan B mirrors it via the existing event stream:

1. Agent's `Driver::transition_to(state)` already calls `inventory.update_deployment_state(...)` AND `self.emit("deployment.<state>", None)`.
2. **Plan B change**: extend `Driver::emit` to include the FULL `Deployment` row (serialized as JSON) in `event.metadata` for every `deployment.*` event. Controller's existing event handler (which already journals events) gets a new branch: when `event.kind` starts with `deployment.`, parse `metadata` as `Deployment` and call `inventory.upsert_deployment_from_remote(...)`.
3. Controller's `Inventory` (which uses the SAME storage crate, hence the SAME migrations) gets a new method `upsert_deployment_from_remote(d: Deployment)` that does an `INSERT OR REPLACE` (idempotent — handles out-of-order events).
4. Dashboard's `/api/v1/deployments?stack_id=X&state=active` reads from controller's local `deployments` table (now populated).
5. Dashboard's existing `/ws/events` continues to broadcast all `deployment.*` events; the frontend re-fetches when one arrives.

**Why same table, not separate "remote" mirror:** Both agent and controller use `Inventory::open(path)` which runs `sqlx::migrate!()`. Both DBs have the schema. Source of truth differs (agent = local writes, controller = synced from agent). Code paths don't need to know which side they're on — the typed `Deployment` is the same struct everywhere.

### Data flow: controller → agent (abort)

1. Dashboard `POST /api/v1/deployments/:id/abort` → controller handler.
2. Controller looks up the deployment's `host_id`, sends `ControllerMessage::AbortDeployment { deployment_id }` over the per-host Sync stream.
3. Agent's Sync receiver dispatches `AbortDeployment` to `Supervisor::handle_abort(deployment_id)`.
4. Supervisor maintains a `HashMap<String, CancellationToken>` (deployment_id → token) populated when each Driver is spawned. Calls `token.cancel()` on the matching entry.
5. The Driver's `tokio::select!` (described below) wakes on the cancel and routes to the abort path.

**Sub-second latency** end-to-end. No heartbeat polling.

### Driver state machine extension

Plan A's machine: `pending → spinning_up → switching → draining → destroying_blue → done` (or `aborted` / `failed` from any pre-terminal state).

Plan B adds:
- A new state **`recovering`** between `draining` and `destroying_blue`, entered only when a post-switch collapse is detected.
- During `draining`, the Driver runs:

```rust
tokio::select! {
    _ = tokio::time::sleep(self.grace_period + self.drain_buffer) => {
        // Happy path: proceed to destroying_blue.
    }
    _ = self.abort_token.cancelled() => {
        // User clicked Abort during drain. Swap back to blue + cleanup green.
        self.recover_to_blue("aborted_during_drain").await?;
        // Skip destroying_blue, mark Aborted, return.
    }
    _ = self.wait_for_green_unhealthy() => {
        // Pingora marked green unhealthy. Try to recover.
        self.transition_to(DeploymentState::Recovering).await?;
        self.recover_to_blue("post_switch_collapse_recovered").await?;
        // Skip destroying_blue, mark Failed (not Aborted — the deployment FAILED, not user-cancelled).
    }
}
```

`wait_for_green_unhealthy()` subscribes to the agent's existing event bus (`routing.upstream.health_changed` events emitted by Plan A's healthchecker eviction code) and returns when one matches `event.metadata.public_hostname == self.deployment.public_hostname` AND `event.metadata.healthy == false`.

### `recover_to_blue` helper

Before calling `swap_upstream(green)`, the Driver snapshots the current `Upstream` (the blue one). On recovery:

1. If `blue_container` is still in Docker (likely — we haven't reached destroying_blue yet): re-call `swap_upstream(state, hostname, blue_upstream_snapshot, grace=0)` — instant swap-back, no second drain.
2. Cleanup green: `docker stop` + `docker rm` on green.
3. Set Deployment row state appropriately (Aborted for user-aborted, Failed for collapse).
4. Emit `deployment.recovered` event with reason in metadata.

If blue is somehow gone (defensive): set Failed with `error: "post_switch_collapse_unrecoverable_blue_destroyed"`, leave green up but already-unhealthy — service is degraded; user gets paged via the existing notifier.

### Abort during pre-drain states

If user aborts during `pending` / `spinning_up`:
- Driver's outer `tokio::select!` (around the whole `run_inner`) checks `abort_token.cancelled()`.
- If green is started: stop+remove it.
- Set Aborted with `error: "aborted_by_user"`. Blue is untouched (was never replaced).

If user aborts during `switching` (the brief moment between healthcheck-pass and swap-call): treat as "too late, complete the switch then go straight to recovery on the other side." Practically, `switching` is a few milliseconds — abort signals during it queue and trigger recovery immediately after swap completes.

### Settings storage (10g)

Migration `0012_services_deploy_strategy.sql`:

```sql
ALTER TABLE services ADD COLUMN deploy_strategy_override TEXT;
-- Values: 'auto' (or NULL), 'blue-green', 'in-place'
```

`Service` struct gets `deploy_strategy_override: Option<String>`. New `Inventory` method:

```rust
pub async fn set_service_deploy_strategy_override(
    &self,
    service_id: ServiceId,
    override_value: Option<&str>,
) -> Result<()>;
```

Plan A's `Supervisor::handle_update_trigger` consults the override AFTER the container label override (label still wins; per Plan A spec). Order:

```
1. trigger.label_strategy is Some(...) → use it
2. service.deploy_strategy_override is Some(...) → use it
3. Auto-detect via classifier
```

The Supervisor needs to look up the service row by `(host_id, stack_id, service_name)`. Add `Inventory::get_service_by_name` if not present.

## Components

### Storage (additive)

- Migration `0012_services_deploy_strategy.sql` — add the column
- `crates/isengard-storage/src/service.rs` — extend Service struct + setter + lookup-by-name
- `crates/isengard-storage/src/deployment.rs` — add `upsert_deployment_from_remote(d)` helper (controller-side use)

### Proto + Sync

- `crates/isengard-proto/proto/isengard.v1.proto`:
  ```proto
  message ControllerMessage {
    oneof payload {
      // ... existing variants ...
      AbortDeployment abort_deployment = <next available field number>;
    }
  }
  message AbortDeployment {
    string deployment_id = 1;
  }
  ```
  (Use the next-available field number; do NOT renumber existing fields.)
- `crates/isengard-controller/src/service.rs` (or wherever the per-host Sync sender is held) — expose `controller.send_to_host(host_id, ControllerMessage)`. Confirm whether this already exists; if not, the registry pattern from Plan A's `RoutingPusher` is the precedent.
- `crates/isengard-agent/src/sync.rs` (or wherever ControllerMessage is dispatched) — match on `AbortDeployment` → call `Supervisor::handle_abort(deployment_id)`.

### Agent: driver + supervisor

- `crates/isengard-agent/src/deployment/driver.rs`:
  - Add `abort_token: tokio_util::sync::CancellationToken` field
  - Snapshot `blue_upstream: Option<Upstream>` before `swap_upstream` call (saved as a Driver field, NOT persisted — recovery only matters within a Driver task lifetime)
  - Outer `run` wraps `run_inner` in a `tokio::select!` against `abort_token.cancelled()`
  - During `draining`: replace plain `sleep` with the 3-way `select!` above
  - New `recover_to_blue(reason: &str) -> Result<()>` helper
  - New `wait_for_green_unhealthy()` helper: subscribes to event bus, filters by hostname
  - New state `DeploymentState::Recovering`
  - Snapshot in metadata: extend `emit()` to serialize the full Deployment row into `event.metadata.deployment` so the controller can upsert
- `crates/isengard-agent/src/deployment/mod.rs`:
  - Supervisor stores `abort_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>`
  - `handle_update_trigger` creates a token, stores it, passes it to `Driver::new`, spawns
  - On Driver completion (success or failure): remove the token from the map (use `Arc<DropGuard>` pattern to make this automatic)
  - New `handle_abort(deployment_id: &str)` method: looks up token, calls `cancel()`, returns whether anything was cancelled
  - Supervisor consults `service.deploy_strategy_override` in `handle_update_trigger` (between label override and auto-detect)

### Agent: event subscriber

The driver needs to listen for `routing.upstream.health_changed` events. Plan A's eviction code emits these. Need an in-process pub-sub, scoped to the agent. Options:
- Use the existing `EventEmitter` if it has a subscribe side. Most likely it doesn't (it's an emit-only sink).
- Add a separate `tokio::sync::broadcast` channel for proxy events; emitter fans out to BOTH the controller-bound emitter AND the local broadcast.

**Decision**: Add a `proxy_events: broadcast::Sender<ProxyEvent>` to the proxy module. Emit on it whenever the eviction code transitions an upstream's `healthy` field. Driver subscribes via `proxy_events.subscribe()` and filters.

This is a small, focused addition. The `ProxyEvent` enum has one variant for v1: `UpstreamHealthChanged { hostname: String, healthy: bool }`.

### Controller: event handler + REST + sync sender

- `crates/isengard-controller/src/event_handler.rs` (or wherever events are journaled): extend to recognize `deployment.*` event kinds, parse `metadata.deployment` as `Deployment`, call `inventory.upsert_deployment_from_remote(d)`.
- `crates/isengard-plugins/dashboard/src/deployments.rs` (NEW):
  - `GET /api/v1/deployments?stack_id=X&state=active|history&limit=N` → JSON list
  - `POST /api/v1/deployments/:id/abort` → looks up the deployment, finds its host_id, sends `ControllerMessage::AbortDeployment { id }` via the per-host Sync sender, returns 202 (the abort is async; client subscribes to WS to learn the outcome)
  - `GET /api/v1/services/:service_id/deploy-strategy` and `PUT` for the override
- Mount under `/api/v1/deployments` and `/api/v1/services/:id/deploy-strategy` in the dashboard router

### Dashboard frontend

- `composables/useDeployments.ts`:
  - `useDeployments(stackId)` returns `{ active, history, refresh, abort(id) }`
  - On mount: GET `/api/v1/deployments?stack_id=X&state=active`
  - WS subscription: on any `deployment.*` event, refresh()
- `composables/useServiceDeployStrategy.ts`:
  - GET / PUT for the override per service
- `components/DeploymentInProgressPanel.vue`:
  - Renders the active Deployment object: progress steps + timestamps + Abort button
  - Calls `abort(deployment.id)` on click
- `components/DeploymentAbortedPanel.vue`:
  - Renders an Aborted/Failed Deployment: error message under "Reason:" (raw string from `Deployment.error`) + a single Retry button.
  - Retry calls `POST /api/v1/stacks/:id/force-update` (existing endpoint, queues `ForceUpdate` host action — triggers updater cycle, which goes through the Plan A → Plan B path again).
  - No "View logs" / "Adjust healthcheck" buttons in v1.
- `components/DeploymentsSettings.vue`:
  - Lists services across all stacks (paginated if >50). Each row: stack name, service name, current override (`auto` / `blue-green` / `in-place` radio), Save button (per-row).
  - Eligibility annotations deferred (see non-goals). For v1, the override radio shows what's stored, not what would be auto-detected.
- `pages/stacks/[id].vue`:
  - Above the Services section, render `<DeploymentInProgressPanel v-if="active" :deployment="active" />` or `<DeploymentAbortedPanel v-else-if="recentlyAborted" :deployment="recentlyAborted" />`
  - "recentlyAborted" = the most recent deployment for this stack within the last 5 minutes whose state is Aborted/Failed; shown until dismissed or a new deployment starts
- `pages/settings/index.vue`:
  - Add a new tab "Deployments" next to "Networking" (use the existing tab pattern from Plan C)

## Edge cases + how this PR handles each

| Scenario | v1 behavior |
|---|---|
| Two services in the same stack deploy simultaneously | Two `Deployment` rows in flight. Stack detail panel shows the most recent one with a small "+1 more" indicator (clickable to cycle). Abort acts on the visible one. |
| User refreshes mid-deployment | `useDeployments` re-fetches on mount; controller has the up-to-date row from the last event. Panel renders immediately. |
| Controller restarts mid-deployment | Controller's `deployments` table loses no data (it's persisted). On reconnect to agent, agent re-emits the next state-change event; controller upserts. The brief window between restart and next event shows stale-but-correct data. |
| Agent restarts mid-deployment | Plan A's `reconcile_orphans` marks the row Failed at startup. Controller receives the `deployment.failed` event, upserts. Dashboard shows the failed panel. |
| Abort fires DURING the swap_upstream call | Treat as "too late." Driver completes the swap, then the post-swap select catches the abort token and triggers recovery (swap back to blue). Same code path as a real post-switch collapse. |
| Abort fires AFTER blue is destroyed (race) | `recover_to_blue` discovers blue's container is gone → sets Failed with `post_switch_collapse_unrecoverable_blue_destroyed`. Service is now down (green is also gone after recovery). User gets paged. Rare, but the row shape captures the truth. |
| User clicks Abort after the deployment already finished (Done/Failed/Aborted) | REST endpoint returns 200 with `{ "noop": true, "reason": "deployment_already_terminal" }`. Frontend swallows. |
| User changes `deploy_strategy_override` for a service that's currently mid-deployment | Override takes effect on the NEXT deployment. The in-flight one continues with whatever strategy it started with. |
| Pingora's health_changed fires unhealthy then healthy in quick succession (flap) | Driver's `wait_for_green_unhealthy` returns on the first `unhealthy`. Recovery proceeds; the late `healthy` is ignored. Conservative — favor uptime. |
| Multiple `deployment.*` events arrive at controller out of order | `upsert_deployment_from_remote` is `INSERT OR REPLACE` keyed on `id`. Each event carries the full row, so latest-write-wins on each field. The agent's events are emitted in causal order (each transition writes the row then emits), so reorder is theoretical but harmless. |
| Settings UI lists services for a stack with no `services` rows yet (stack just created) | Empty state: "No services discovered yet on this stack. Run a deployment or wait for the next agent heartbeat." |

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Broadcasting full `Deployment` row in event metadata bloats events | The row is small (~500 bytes serialized). Acceptable. If it grows, can switch to `id` + a separate query — but YAGNI. |
| Driver's tokio::select! has subtle ordering bugs (e.g., abort fires AND grace completes simultaneously) | Test both orderings explicitly in unit tests. tokio's select! is biased toward the first arm by default; we want fair selection — use `tokio::select!` with `biased;` directive only if we have a deliberate priority (we don't). |
| Recovery's instant `swap_upstream(grace=0)` might cause connection blips | Plan C's `swap_upstream` removes the old upstream after grace. With grace=0, the cleanup task fires immediately, but in-flight requests on green continue against green (the proxy's connection pool; not our problem). New requests route to blue. Acceptable for v1. |
| Controller doesn't know which agents are connected → can't queue abort if agent is offline | Same problem as routing rule pushes. Solution mirrors Plan A's `RoutingPusher`: keep a `host_id → mpsc::Sender<ControllerMessage>` map; `send_to_host` returns Err if the host isn't connected. REST handler returns 503 with body explaining the offline agent. |
| `services.deploy_strategy_override` interacts confusingly with the container label | UI shows BOTH: "Override: <radio>" and "Container label: blue-green (overrides settings)". Documents the precedence inline. |

## Testing

### Unit tests (storage + agent)

- `crates/isengard-storage/src/service.rs` — set + get override (2 tests)
- `crates/isengard-storage/src/deployment.rs` — `upsert_deployment_from_remote` is idempotent (1 test)
- `crates/isengard-agent/src/deployment/driver.rs`:
  - Recovery path: green unhealthy during drain → swap back + Failed (1 test, mock both Pingora event source and DriverDeps)
  - Abort during pending: cancel before spinup → Aborted (1 test)
  - Abort during spinning_up: cancel after green starts → cleanup + Aborted (1 test)
  - Abort during draining: cancel during select → swap back + Aborted (1 test)
- `crates/isengard-agent/src/deployment/mod.rs`:
  - Supervisor stores token on spawn, removes on drop (1 test)
  - `handle_abort` for unknown id returns `false`; for known id returns `true` (1 test)
  - Supervisor consults service.deploy_strategy_override (1 test, mock service lookup)

Total: ~10 new unit tests.

### Integration tests (controller + REST)

- `crates/isengard-plugins/dashboard/tests/deployments_endpoints.rs`:
  - `GET /api/v1/deployments?stack_id=X` returns active deployments
  - `GET ?state=history` returns terminal-state deployments (limit + ORDER BY)
  - `POST /api/v1/deployments/:id/abort` returns 202 + sends ControllerMessage to mock sender
  - `POST` on terminal deployment returns noop response
  - Total: 4 tests

### Real-Docker e2e (deferred to later PR or manual smoke for this PR)

The Plan A e2e suite already covers the happy path end-to-end. Adding real-Docker e2e for abort + post-switch collapse is significant work (need to inject Pingora-side health failures mid-test). Defer to a follow-up — manual smoke checklist documented in the PR body.

## Phasing inside Plan B

| Task | Sub-phase | Scope | Tests |
|---|---|---|---|
| 1 | 10g-storage | Migration 0012 + service column + setter + getter + Supervisor consults override | 2 storage + 1 supervisor |
| 2 | 10e-agent | Extend Driver::emit to put Deployment row in metadata | (manual smoke) |
| 3 | 10e-controller | Event handler upserts deployment metadata + `upsert_deployment_from_remote` storage method | 1 storage + 1 controller |
| 4 | 10e-rest | `GET /api/v1/deployments?stack_id=&state=` endpoint | 2 endpoint tests |
| 5 | 10e-frontend | useDeployments composable + DeploymentInProgressPanel + Stack detail wire-up | (manual smoke; bun build) |
| 6 | 10f-driver | Recovering state + tokio::select with abort_token + health-event listener + swap_back_to_blue + ProxyEvent broadcast | 4 driver unit tests + 2 supervisor |
| 7 | 10f-proto-sync | Proto AbortDeployment + controller send_to_host + agent dispatch to Supervisor::handle_abort | 1 sync test |
| 8 | 10f-rest | `POST /api/v1/deployments/:id/abort` endpoint | 2 endpoint tests |
| 9 | 10f-frontend | DeploymentAbortedPanel + abort button wired + Stack detail wire-up | (manual smoke) |
| 10 | 10g-frontend | DeploymentsSettings.vue + new tab in /settings + composable | (manual smoke) |
| 11 | 10h | Final workspace gates + open PR #22 | (gates) |

## Implementation dependencies

- Plan A's `proxy::healthcheck` eviction code (already on branch via stack — also need to confirm it actually emits `routing.upstream.health_changed` events; spec says it does, plan implementation should be checked)
- Plan A's `Deployment` entity + state machine (this branch's base)
- Plan C's `swap_upstream` (also on branch via stack)
- `tokio_util` crate for `CancellationToken` — likely already in workspace; if not, add to `isengard-agent`'s Cargo.toml

## Success criteria

- All 10 implementation tasks committed
- ~16 unit tests + 4 controller integration tests green
- `cargo build --workspace` clean
- `cargo test --workspace` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo deny check` clean
- `bun run build` clean
- PR #22 open against PR #21 (`feat/blue-green-core`)
- Manual smoke checklist in PR body:
  - [ ] Trigger deployment via real Docker — DeploymentInProgressPanel renders + updates live
  - [ ] Click Abort during spinning_up — cleanup happens within 1s
  - [ ] Click Abort during draining — traffic swaps back to blue immediately
  - [ ] Kill green container manually mid-drain — collapse detected via Pingora, swap back, panel shows Failed with reason
  - [ ] Set override "in-place" on a routed service in Settings — next update goes through recreate path
