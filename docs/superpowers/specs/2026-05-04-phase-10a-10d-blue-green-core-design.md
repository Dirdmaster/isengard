# Phase 10 Plan A — Blue-Green Deployment Core (10a-10d)

**Status:** spec
**Phase:** 10a-10d
**Date:** 2026-05-04
**Stacked on:** PR #20 (`feat/networking-settings-ui-and-swap`, Plan C)
**Vault design:** `1 Projects/Isengard/Blue-Green Deployment.md`

## What this PR builds

A working blue-green deployment driver in the agent: when the `updater` plugin detects that a routed, healthcheck-equipped container needs an update, the driver spins up a green container alongside blue, waits for green to go healthy, calls `proxy::swap_upstream` to atomically shift traffic, drains and removes blue. On failure, the driver leaves blue serving and emits an abort event.

What this is **not**: no UI panel, no settings page, no per-rule healthcheck-spec config field, no per-rule connection-lifetime knob, no resource pre-flight check, no post-switch swap-back recovery, no multi-host rolling. Those land in subsequent PRs (Plan B = 10e-10g, Plan C = 10h-10i).

## Goals

1. Make blue-green Just Work for the typical case: a routed stateless web service with a healthcheck.
2. Reuse Plan A's `proxy::healthcheck::HealthChecker` as the probe primitive (one healthcheck implementation in the codebase, not two).
3. Reuse Plan C's `proxy::swap_upstream` as the traffic-shift primitive (atomic swap with grace-period drain, already battle-tested by 6 unit tests).
4. Keep the eligibility classifier pure (table-test friendly) and the state machine driver event-loop free of business logic.
5. Real-Docker e2e coverage for the happy path and the most common failure (healthcheck timeout).

## Non-goals (deferred to later PRs)

| Deferred | Why deferred | Where it lands |
|---|---|---|
| In-flight progress UI on Stack detail | Visual surface, no behavior change | Plan B (10e) |
| Failure UI ("Healthcheck did not pass…" panel + Retry) | Requires Plan B's progress panel | Plan B (10f) |
| Settings → Deployments per-service strategy override | UX surface, not core machinery | Plan B (10g) |
| Deployment history tab | Read-only audit | Plan C (10h) |
| Multi-host rolling parallelism | Cross-host orchestration, large surface | Plan C (10i) |
| Per-rule `connection_lifetime_strategy: drain \| force_close` knob | Open question #1 in design — defer until a real WebSocket service breaks | Future PR |
| Per-rule healthcheck-spec config (`success_threshold`, `deadline_secs`, etc. as routing-rule columns) | v1 uses defaults baked into the deployment driver. Per-rule config requires schema change + UI. | Plan B/C |
| Resource pre-flight (memory/disk check before green-start) | Docker's start error is more specific than what we'd write. Cgroup-version reliability is poor across Docker Desktop / OrbStack. | Future PR |
| Post-switch collapse swap-back (green dies after taking traffic, before blue is destroyed) | Rare; design says "scary case". v1 emits `deployment.failed`; Plan B can add the recovery. | Plan B (10f) |
| Sticky-session / WebSocket / SSE handling | Open question #1 — same as connection-lifetime knob | Future PR |
| Database migration awareness (expand-contract pattern) | Open question #3 — not our problem per design | Never |
| Pre-deploy hooks (cache warming, smoke tests) | Couples to future Hooks plugin | Phase 12+ |

## Architecture

### Module placement

The deployment driver lives in `crates/isengard-agent/src/deployment/`, sibling to `proxy/`. **Not** as a plugin, because the driver needs in-process access to `proxy::swap_upstream` — a plugin can't depend on `isengard-agent` (the agent loads the plugin, so a back-dep would be circular).

```
crates/isengard-agent/src/
├── proxy/
│   ├── healthcheck.rs          (existing, Plan A)
│   ├── swap.rs                 (existing, Plan C)
│   └── upstreams.rs            (existing)
└── deployment/                 (NEW)
    ├── mod.rs                  (Supervisor: tokio task per active Deployment row)
    ├── eligibility.rs          (pure classifier)
    ├── healthcheck.rs          (DeploymentHealthcheck wrapper)
    ├── state.rs                (DeploymentState enum + transition helpers)
    └── driver.rs               (per-deployment state machine task)
```

### Updater → driver bridge

The existing `updater` plugin already detects "needs update" containers and calls `recreate.rs` directly. This PR adds a branch:

1. Updater detects `needs_update` (existing).
2. Updater emits a new `container.update_needed` Event via the existing `EventEmitter` it already has.
3. The agent's deployment Supervisor subscribes to this event.
4. Supervisor runs `eligibility::classify(container, label_override)`.
5. **`InPlace`** branch: emits `container.update_in_place` event back. The updater plugin (also subscribed) calls its existing `recreate.rs` path. **No behavior change for in-place containers.**
6. **`BlueGreen`** branch: Supervisor inserts a `Deployment` row, spawns a driver task, returns.

Why event-bus and not direct trait call: keeps the updater plugin's dep surface unchanged (it imports `isengard-core` only, not `isengard-agent`). The cost is one event-emit + one event-receive per update. Acceptable given the natural cadence (updater runs every 30s).

### Driver state machine

The driver is a tokio task that owns one `Deployment` row and walks states:

```
pending
  ↓ (always)
spinning_up
  ↓ docker create + start green
  ↓ DeploymentHealthcheck.wait_for_healthy()
  ├─ Ok(passed_at) → switching
  └─ Err(timeout)  → aborted (cleanup green, emit deployment.aborted)

switching
  ↓ proxy::swap_upstream(host, green_upstream, grace_period)
  ↓ (returns immediately)
draining
  ↓ tokio::time::sleep(grace_period + small_buffer)
  ↓
destroying_blue
  ↓ docker stop + docker rm blue
  ↓
done
  emit deployment.completed
```

Each state transition writes to the `Deployment` row (timestamp + state field) and emits an event. If the agent restarts mid-deployment, the Supervisor on startup loads all rows in non-terminal states and resumes them — but for v1, we do the simpler thing: rows stuck in `spinning_up` / `switching` / `draining` after restart are marked `failed` with `error: agent_restarted_during_deployment`. Resume-on-restart is a Plan B/C concern.

### Atomic swap call

The driver calls `proxy::swap_upstream` directly, in-process:

```rust
// Inside driver.rs, when transitioning switching → draining:
let new_upstream = Upstream {
    container_id: deployment.green_container.clone().unwrap(),
    addr: green_addr,
    healthy: true,                         // we just verified
    health_path: deployment.health_path.clone(),
    health_interval: Duration::from_secs(5),
    consecutive_failures: 0,
    state: UpstreamState::Active,
};
let grace = Duration::from_secs(60);       // v1 default, hard-coded
proxy::swap_upstream(&proxy_state, &deployment.public_hostname, new_upstream, grace).await?;
deployment.set_state(DeploymentState::Draining).await?;
deployment.switched_at = Some(Utc::now());
```

The driver then sleeps `grace + 5s` (the small buffer covers any clock skew between the swap_upstream cleanup task and our own timer) before transitioning to `destroying_blue`.

## Components

### `eligibility.rs`

Pure function. No async, no IO, no Docker calls — takes the already-fetched container spec.

```rust
pub enum DeployStrategy { BlueGreen, InPlace }

#[derive(Debug, PartialEq)]
pub enum InPlaceReason {
    NoRoutingRule,
    StatefulVolume,
    NoHealthcheck,
    LabelForced,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    BlueGreen,
    InPlace { reason: InPlaceReason },
}

pub struct ContainerSpec<'a> {
    pub has_routing_rule: bool,           // routing rule exists for this service:port
    pub has_healthcheck: bool,            // image has HEALTHCHECK or compose has healthcheck
    pub rw_volume_mounts: &'a [String],   // bind/named mounts in rw mode
    pub label_strategy: Option<&'a str>,  // value of isengard.deploy.strategy label
}

pub fn classify(spec: &ContainerSpec) -> Decision {
    // Label override takes precedence
    match spec.label_strategy {
        Some("blue-green") => return Decision::BlueGreen,
        Some("in-place")   => return Decision::InPlace { reason: InPlaceReason::LabelForced },
        Some("auto") | None => {}
        Some(_) => {} // unknown value: fall through to autodetect
    }

    // Autodetect cascade
    if !spec.has_routing_rule { return Decision::InPlace { reason: InPlaceReason::NoRoutingRule }; }
    if !spec.rw_volume_mounts.is_empty() { return Decision::InPlace { reason: InPlaceReason::StatefulVolume }; }
    if !spec.has_healthcheck { return Decision::InPlace { reason: InPlaceReason::NoHealthcheck }; }

    Decision::BlueGreen
}
```

Test cases (4): each `InPlaceReason` + the BlueGreen happy path. Plus a 5th: label override wins over autodetect.

### `healthcheck.rs`

Wraps `proxy::healthcheck::HealthChecker` with polling + thresholds + deadline. Does NOT replace HealthChecker — composes it.

```rust
use crate::proxy::healthcheck::HealthChecker;
use chrono::{DateTime, Utc};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::sleep;

pub struct DeploymentHealthcheck {
    inner: HealthChecker,
    interval: Duration,
    success_threshold: u32,
    initial_delay: Duration,
    deadline: Duration,
}

impl DeploymentHealthcheck {
    pub fn new(inner: HealthChecker) -> Self {
        Self {
            inner,
            interval: Duration::from_secs(5),
            success_threshold: 2,
            initial_delay: Duration::from_secs(0),
            deadline: Duration::from_secs(120),
        }
    }
    pub fn with_interval(mut self, d: Duration) -> Self { self.interval = d; self }
    pub fn with_success_threshold(mut self, n: u32) -> Self { self.success_threshold = n; self }
    pub fn with_initial_delay(mut self, d: Duration) -> Self { self.initial_delay = d; self }
    pub fn with_deadline(mut self, d: Duration) -> Self { self.deadline = d; self }
}

#[derive(Debug)]
pub struct AttemptResult {
    pub at: DateTime<Utc>,
    pub passed: bool,
}

#[derive(Debug)]
pub struct HealthcheckTimeout {
    pub last_attempts: Vec<AttemptResult>,   // up to last 5
}

impl DeploymentHealthcheck {
    /// Polls until success_threshold consecutive passes or deadline elapses.
    /// Returns Ok(passed_at) or Err(HealthcheckTimeout).
    pub async fn wait_for_healthy(&self, addr: SocketAddr) -> Result<DateTime<Utc>, HealthcheckTimeout> {
        sleep(self.initial_delay).await;
        let started = std::time::Instant::now();
        let mut consecutive = 0u32;
        let mut last_attempts: Vec<AttemptResult> = Vec::new();

        loop {
            if started.elapsed() >= self.deadline {
                return Err(HealthcheckTimeout { last_attempts });
            }
            let passed = self.inner.check_once(addr).await;
            let attempt = AttemptResult { at: Utc::now(), passed };
            last_attempts.push(attempt);
            if last_attempts.len() > 5 { last_attempts.remove(0); }

            if passed {
                consecutive += 1;
                if consecutive >= self.success_threshold {
                    return Ok(Utc::now());
                }
            } else {
                consecutive = 0;
            }
            sleep(self.interval).await;
        }
    }
}
```

Test cases (with mock HealthChecker behavior — see Testing section): immediate pass after threshold, fail-then-pass-then-fail-counter-resets, deadline-exceeds-with-no-passes, initial_delay-honored, last_attempts-capped-at-5.

### `state.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum DeploymentState {
    Pending,
    SpinningUp,
    Switching,
    Draining,
    DestroyingBlue,
    Done,
    Aborted,
    Failed,
}

impl DeploymentState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted | Self::Failed)
    }
}
```

For the in-place strategy we don't write a Deployment row at all — the existing recreate.rs flow stays untouched. (Optional: write an `InPlace` deployment row for audit. Defer to Plan B's history tab.)

### `driver.rs`

A `Driver` struct owns one Deployment row + the side effects to advance it:

```rust
pub struct Driver {
    deployment: Deployment,
    docker: Arc<bollard::Docker>,
    proxy_state: ProxyState,
    storage: Arc<DeploymentStore>,
    emitter: Arc<dyn EventEmitter>,
}

impl Driver {
    pub async fn run(mut self) {
        if let Err(e) = self.run_inner().await {
            // run_inner returns Err only for unrecoverable errors after we've already entered
            // an active state. Mark Failed.
            self.fail(format!("{e:?}")).await;
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        self.transition(DeploymentState::SpinningUp).await?;
        let green_addr = self.start_green().await?;            // returns Err on docker failure → caller aborts
        let hc = DeploymentHealthcheck::new(self.build_health_checker());
        match hc.wait_for_healthy(green_addr).await {
            Ok(passed_at) => {
                self.deployment.healthcheck_passed_at = Some(passed_at);
                self.transition(DeploymentState::Switching).await?;
                self.do_swap(green_addr).await?;
                self.transition(DeploymentState::Draining).await?;
                tokio::time::sleep(self.grace_period() + Duration::from_secs(5)).await;
                self.transition(DeploymentState::DestroyingBlue).await?;
                self.destroy_blue().await?;
                self.transition(DeploymentState::Done).await?;
                self.emit("deployment.completed");
                Ok(())
            }
            Err(timeout) => {
                self.deployment.error = Some(format!("healthcheck_timeout: {} attempts logged", timeout.last_attempts.len()));
                self.cleanup_green().await.ok();           // best-effort
                self.transition(DeploymentState::Aborted).await?;
                self.emit_abort("healthcheck_timeout", &timeout);
                Ok(())
            }
        }
    }

    async fn start_green(&mut self) -> Result<SocketAddr> { /* docker create + start, return addr */ }
    async fn cleanup_green(&mut self) -> Result<()> { /* docker stop + rm, ignore "not found" */ }
    async fn destroy_blue(&mut self) -> Result<()> { /* docker stop + rm */ }
    async fn do_swap(&mut self, green_addr: SocketAddr) -> Result<()> { /* call proxy::swap_upstream */ }
    async fn transition(&mut self, new: DeploymentState) -> Result<()> { /* update row, emit deployment.<state> */ }
    async fn fail(&mut self, msg: String) { /* terminal state Failed */ }
    fn build_health_checker(&self) -> HealthChecker { /* HTTP if path else TCP */ }
    fn grace_period(&self) -> Duration { Duration::from_secs(60) }
}
```

If `start_green` returns Err immediately (image pull failure, immediate exit), `run_inner` propagates it; `run()`'s top-level catch sets state to `Aborted` (not Failed — green never took traffic) with `error: spinup_failed: <msg>`, emits `deployment.aborted`. (Implementation detail for the plan: probably handle this in `run_inner` directly so we can pick the right terminal state instead of always-Failed.)

### `mod.rs` (Supervisor)

```rust
pub struct DeploymentSupervisor {
    docker: Arc<bollard::Docker>,
    proxy_state: ProxyState,
    storage: Arc<DeploymentStore>,
    emitter: Arc<dyn EventEmitter>,
    inventory: Inventory,                    // for routing-rule lookup, healthcheck spec
}

impl DeploymentSupervisor {
    /// Called when the updater plugin emits container.update_needed.
    /// Decides strategy. For BlueGreen, inserts a row + spawns a Driver task.
    /// For InPlace, emits container.update_in_place (the updater plugin re-handles).
    pub async fn handle_update_needed(&self, event: ContainerUpdateNeeded) -> Result<()> { ... }

    /// Called at agent startup. Marks any non-terminal Deployment rows as Failed
    /// with reason agent_restarted_during_deployment. (v1 simplification — Plan B/C
    /// can add resume-on-restart.)
    pub async fn reconcile_orphans(&self) -> Result<()> { ... }
}
```

## Storage

### Migration `0011_deployments.sql`

```sql
CREATE TABLE deployments (
    id                       TEXT PRIMARY KEY,
    host_id                  BLOB NOT NULL,
    stack_id                 INTEGER REFERENCES stacks(id) ON DELETE CASCADE,
    service_name             TEXT NOT NULL,
    strategy                 TEXT NOT NULL CHECK (strategy IN ('blue-green', 'in-place')),
    state                    TEXT NOT NULL,
    blue_container           TEXT,
    green_container          TEXT,
    blue_digest              TEXT NOT NULL,
    green_digest             TEXT NOT NULL,
    public_hostname          TEXT,                       -- snapshot at deploy time
    health_path              TEXT,                       -- snapshot of routing rule's healthcheck path; NULL = TCP-only
    container_port           INTEGER,                    -- snapshot of routed port (so we can build the green addr)
    healthcheck_started_at   TEXT,
    healthcheck_passed_at    TEXT,
    switched_at              TEXT,
    drained_at               TEXT,
    finished_at              TEXT,
    error                    TEXT,
    metadata_json            TEXT,
    created_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_deployments_state_active
    ON deployments(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

CREATE INDEX idx_deployments_stack_created
    ON deployments(stack_id, created_at DESC);
```

`public_hostname` added to the design doc's schema — needed at swap time without a routing-rules join.

### `crates/isengard-storage/src/deployment.rs`

```rust
pub struct Deployment {
    pub id: String,
    pub host_id: HostId,
    pub stack_id: i64,
    pub service_name: String,
    pub strategy: DeployStrategy,
    pub state: DeploymentState,
    pub blue_container: Option<String>,
    pub green_container: Option<String>,
    pub blue_digest: String,
    pub green_digest: String,
    pub public_hostname: Option<String>,
    pub health_path: Option<String>,
    pub container_port: Option<i64>,
    pub healthcheck_started_at: Option<DateTime<Utc>>,
    pub healthcheck_passed_at: Option<DateTime<Utc>>,
    pub switched_at: Option<DateTime<Utc>>,
    pub drained_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub metadata_json: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct DeploymentStore<'a> { /* &'a SqlitePool wrapper */ }

impl<'a> DeploymentStore<'a> {
    pub async fn insert(&self, d: &NewDeployment) -> Result<Deployment>;
    pub async fn update_state(&self, id: &str, state: DeploymentState) -> Result<()>;
    pub async fn set_green_container(&self, id: &str, container: &str) -> Result<()>;
    pub async fn set_healthcheck_passed(&self, id: &str, at: DateTime<Utc>) -> Result<()>;
    pub async fn set_switched(&self, id: &str, at: DateTime<Utc>) -> Result<()>;
    pub async fn set_drained(&self, id: &str, at: DateTime<Utc>) -> Result<()>;
    pub async fn set_finished(&self, id: &str, at: DateTime<Utc>) -> Result<()>;
    pub async fn set_error(&self, id: &str, error: &str) -> Result<()>;
    pub async fn get(&self, id: &str) -> Result<Option<Deployment>>;
    pub async fn list_in_flight(&self, host_id: &HostId) -> Result<Vec<Deployment>>;
    pub async fn list_in_flight_for_service(&self, host_id: &HostId, service_name: &str) -> Result<Vec<Deployment>>;
    pub async fn list_by_stack(&self, stack_id: i64, limit: u32) -> Result<Vec<Deployment>>;
    pub async fn mark_orphans_failed(&self, host_id: &HostId, reason: &str) -> Result<u64>;  // Supervisor's reconcile_orphans uses this
}
```

Each `set_*` method updates `updated_at` automatically. Typed setters preferred over a generic `update_field` to keep the call sites self-documenting and avoid the implementer reaching for serde gymnastics.

Test cases (4): insert returns row with generated ULID; update_state changes state and updated_at; list_in_flight excludes terminal states; FK cascade deletes deployments when parent stack is dropped.

## Updater integration

### Event added to `isengard-core/src/event.rs` (or wherever events live)

```rust
pub struct ContainerUpdateNeeded {
    pub container_id: String,
    pub service_name: String,
    pub stack_id: i64,
    pub host_id: HostId,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,        // e.g. "blog/web:1.3.0" — driver uses this for `docker create`
}
```

If the updater already emits a similar event for any other consumer (e.g., the dashboard's events tab), prefer to extend that event over adding a new one. The plan's first task is to grep the updater for existing emit calls; the spec assumes a fresh event but accepts reuse.

### Updater plugin change

In the updater's existing classification loop, when a container is classified as `needs_update`, instead of immediately calling `recreate::recreate_container`, emit the event. The updater plugin also subscribes to `container.update_in_place` and calls `recreate::recreate_container` when received.

This means:
- BlueGreen-eligible: updater emits `update_needed` → supervisor decides BG → driver runs the BG flow → blue gets destroyed at the end (driver does the docker stop+rm, NOT the updater's recreate.rs)
- InPlace: updater emits `update_needed` → supervisor decides InPlace → emits `update_in_place` back → updater's recreate.rs runs (existing flow)

Net result: existing in-place behavior unchanged. New BG behavior added.

## Testing

### Unit tests

| File | Tests | Count |
|---|---|---|
| `eligibility.rs` | each InPlaceReason, BlueGreen happy, label override priority | 5 |
| `healthcheck.rs` | immediate pass after threshold; fail-pass-fail counter resets; deadline timeout; initial_delay honored; last_attempts capped at 5 | 5 |
| `state.rs` | `is_terminal` covers all 8 states; transition helpers | 2 |
| `driver.rs` (with mock docker + mock proxy) | happy path advances states; healthcheck timeout marks Aborted; spin-up failure marks Aborted; swap failure marks Failed | 4 |
| `storage/deployment.rs` | insert; update_state; list_in_flight excludes terminal; cascade delete | 4 |

Total: ~20 unit tests.

### Real-Docker integration tests (`#[ignore]`-gated)

Both opt-in via `cargo test -- --ignored`:

1. **`deployment_blue_green_happy.rs`**: Spin up nginx (blue), apply routing rule, trigger an update to a different nginx tag (green), assert that within ~30s: the deployment row reaches `done`, the routing rule resolves to the green container's IP, blue container is gone, exactly one `deployment.completed` event fired.

2. **`deployment_blue_green_aborts_on_healthcheck.rs`**: Spin up nginx (blue), apply routing rule with healthcheck path `/healthz`, trigger an update to an image whose `/healthz` returns 503. Assert that within `deadline_secs + buffer` (~125s): the deployment row reaches `aborted` with `error` containing "healthcheck_timeout", green container is gone, blue container still serving, blue's routing rule entry unchanged, `deployment.aborted` event fired with the correct reason.

These follow the pattern of Plan A's `proxy_label_e2e.rs` — gated behind `--ignored` because they need a real Docker daemon, kept out of the default test sweep.

## Phasing inside Plan A (4 sub-tasks)

| Task | Files | Tests added | Commit message draft |
|---|---|---|---|
| **10a** | migration 0011 + `storage/src/deployment.rs` (entity + DAO) + `state.rs` (enum, since the storage layer needs it) | 4 storage tests + 2 enum tests | `feat(storage): deployments table + Deployment entity + DAO` |
| **10b** | `deployment/eligibility.rs` + `deployment/healthcheck.rs` | 5 eligibility + 5 healthcheck = 10 | `feat(agent): deployment eligibility classifier + healthcheck wrapper` |
| **10c** | `deployment/driver.rs` + `deployment/mod.rs` (Supervisor) | 4 driver tests + happy-path real-Docker e2e | `feat(agent): blue-green deployment driver + supervisor` |
| **10d** | Updater plugin: emit `container.update_needed`, subscribe to `container.update_in_place`. Wire Supervisor into agent's main task spawn. | abort-case real-Docker e2e | `feat(updater+agent): bridge updater into deployment supervisor` |

Plus a 10e meta-step inside the plan: workspace-green gates + open PR #21 stacked on PR #20.

## Edge cases + how this PR handles each

| Scenario | v1 behavior |
|---|---|
| Updater detects update needed but routing-rule lookup fails | InPlace path (treated as `NoRoutingRule`) |
| Container has both blue-green label AND a stateful volume | Label wins (`Decision::BlueGreen`) — user explicitly opted in, we trust them. Document in the eligibility module. |
| Multiple services in same stack updating simultaneously | One Deployment row per service, one Driver task per row. Pingora handles per-rule swaps independently (Plan C already supports this). |
| User manually stops the blue container during deployment | Driver's `destroy_blue` step gets "container not found" from Docker — treated as success (idempotent). |
| User manually deletes the routing rule during deployment | `swap_upstream` is a no-op on a missing hostname. Driver still drains and destroys blue. The new green has nothing routing to it — observable as a service outage, but not a crash. (Plan B can add a guard: re-check routing rule before swap.) |
| Agent crashes mid-deployment | On restart, Supervisor's `reconcile_orphans` marks any non-terminal Deployment rows as `Failed` with `error: agent_restarted_during_deployment`. v1 doesn't resume — the user re-runs the deploy. (Plan B/C: resume from state.) |
| Deployment for a service that's already mid-deployment | Supervisor checks `list_in_flight(host_id, service_name)` before spawning. If a Driver is already running for this service, log + skip. Updater retries on the next cycle (30s). |
| Image pull fails during `docker create` | `start_green` returns Err. `run_inner` catches → set Aborted with `error: spinup_failed: <docker error>`, emit `deployment.aborted`. Blue untouched. |

## Implementation dependencies

- Plan C's `proxy::swap_upstream` (already on this branch, since we stacked on `feat/networking-settings-ui-and-swap`).
- Plan A's `proxy::healthcheck::HealthChecker` (already present, sibling commit on Plan A's branch — also on this branch via the stack).
- `bollard` Docker client (existing, used by updater).
- `sqlx` SQLite (existing).
- No new external crate dependencies anticipated.

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Driver task panics, deployment row stays in non-terminal state forever | tokio::spawn the Driver inside `tokio::task::Builder::new().spawn()` with a panic handler that calls Supervisor's `mark_failed_with_panic(deployment_id)`. v1 simplification: catch panics inside `Driver::run()` itself with `std::panic::AssertUnwindSafe` + `FuturesExt::catch_unwind`. |
| `swap_upstream` returns success but Pingora hasn't actually rebalanced yet (race) | swap_upstream's contract per Plan C: by the time it returns, the registry is updated. Router consults the registry on each connection. Race window is one in-flight connection → routes to blue. Acceptable. |
| Real-Docker e2e tests are flaky in CI | Keep them `#[ignore]`-gated. Run via the existing real-docker job pattern (Plan A established this). |
| Supervisor's event-loop and the updater's emit are out of sync | Both use the existing in-process `EventEmitter` (sync emit). No queue, no lag. Worst case: Supervisor handler is slow → updater emits twice → Supervisor's `list_in_flight` dedupe catches it. |

## Out-of-scope explicitly called out (so the implementer doesn't gold-plate)

- No `Deployment` row for in-place strategy (per design — InPlace is the existing path, no audit trail change in this PR).
- No `force_close` vs `drain` knob — grace_period default is force-close-after-60s, hard-coded.
- No retry button (no UI).
- No deployment cancellation API (no UI to invoke it from).
- No alerting on aborts beyond the `deployment.aborted` event (notifier plugin is a different surface).
- No metrics/Prometheus surface.

## Open questions resolved at brainstorm time

| Design doc question | Resolution for this PR |
|---|---|
| Sticky sessions / WebSockets / SSE | Force-close after grace, no per-rule knob. |
| Multi-host blue-green | Single host only (single Deployment per service per host). Multi-host = Plan C (10i). |
| Database migrations (expand-contract) | Not our problem. |
| Pre-deploy hooks | Defer to Hooks plugin (Phase 12+). |
| Resource pre-flight | Skip — let Docker fail at green-start. |
| Visual collapse of 5+ events into one timeline entry | Defer to Plan B (10e UI). |

## Success criteria

This PR ships when:

- All 4 sub-tasks committed
- ~20 unit tests green
- Both real-Docker e2e tests pass when run with `--ignored` against a local Docker daemon
- `cargo build --workspace` clean
- `cargo test --workspace` clean (default suite, e2e excluded by default)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo deny check` clean
- PR #21 open against PR #20 (`feat/networking-settings-ui-and-swap`)
- Manual smoke: trigger an update on a real container, observe the deployment row + events, see traffic shift in the dashboard's events tab.
