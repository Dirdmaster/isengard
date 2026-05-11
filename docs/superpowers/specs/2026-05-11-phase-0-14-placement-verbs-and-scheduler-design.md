# Phase 0.14: Native Placement Verbs + Scheduler (design)

Status: proposed
Phase: v0.4 foundation, phase 0.14
Author: 2026-05-11
Related: brainstorm in `Daily/2026-05-10.md` ("UX brainstorm for fleet + install + compose" and "Operator direction").

## Problem

Today the controller has no real scheduler. A stack imported by an agent is "owned" by that agent for life. Multi-host placement is implicit: operators set host hints in compose labels or rely on `compose.yaml` living on the right machine. There is no way to say "give me three replicas spread across the fleet," no way to say "run this on every host," no way to react when a host disconnects.

The 2026-05-10 UX brainstorm locked four native placement verbs simpler than Swarm's `deploy.placement` block: `spread: N`, `global: true`, `on: <host>`, `where: <label-selector>`. The compose parser already accepts the verb tokens without rejecting them (Phase 0.9 `parse_toml_with_native_placement_verbs_passes_through`); they are inert. Phase 0.14 wires them through the parser, models the desired placement state, and ships a scheduler that can satisfy `spread` and `global` across an enrolled fleet and reconcile after host loss.

## Goal

A compose file that declares `spread: 3` on a service results in three running replicas across at most three distinct hosts. If a host disconnects, after a grace period the scheduler reschedules the missing replica onto another healthy host. `global: true` places exactly one replica on every healthy agent, including future enrollees. `on: <host>` pins; if the host is unavailable the service stays Unavailable, not relocated. `where: <selector>` filters which hosts are eligible for any of the above.

### Done bar

- `compose.toml` and `compose.yaml` parsers populate a new `placement: Option<Placement>` field on `DesiredService`.
- The agent forwards labels from `/etc/isengard/agent.toml` and `ISENGARD_LABEL_*` env vars to the controller on every heartbeat.
- A scheduler module in `isengard-controller` resolves each service's `Placement` to a concrete set of `(host_id, replica_index)` assignments, persists them to a new `placements` table, and drives the existing per-host stack apply path with that assignment.
- `isd placement show <stack>` prints the assignment grid. `isd placement explain <stack>/<service>` prints the reasoning (eligible hosts, why each was selected or rejected).
- Disconnect of a host for >60s causes any `spread:`/`global:`-placed replicas owned by that host to reschedule onto remaining eligible hosts.
- Swarm-style `deploy.replicas: N` + `deploy.placement.constraints: [...]` keeps working, translated to the same internal `Placement` model.
- Existing services deployed without verbs keep their current placement.
- Stack stays `Pending` with a clear event when no host matches the filter.

### Non-goals

- Autoscaling (CPU/memory-driven replica count change).
- Gang scheduling (coordinated placement across services in a stack).
- Cost-based placement, bin-packing, spread by AZ.
- Replica migration on health (we reschedule on disconnect, NOT on container OOM/crash; existing per-host restart policies handle that).
- Live label updates (label change on a running agent does not trigger re-placement; operator re-deploys to apply).
- Affinity / anti-affinity between services (this is gang scheduling under another name).
- Topology spread by zone, rack, region.

## Locked verbs (from brainstorm)

In a compose service, exactly zero or one of `spread`, `global`, `on` may appear. `where:` is a modifier that combines with any of the three OR appears alone (acts as "singleton with eligibility filter").

| Verb | Type | Meaning |
|---|---|---|
| _(none)_ | n/a | Singleton. One replica, scheduler picks any eligible host. |
| `spread: N` | int >= 1 | N replicas. Prefer one-per-host; allow multiple-per-host when fleet < N. |
| `global: true` | bool | One replica per eligible host. New enrollees get a replica automatically. |
| `on: <hostname>` | string | Pinned. Singleton; only ever placed on this host. |
| `where: "<selector>"` | string | Label selector. Combines with any of the above. Default: all healthy hosts eligible. |

### Selector grammar (subset of k8s label-selector syntax)

```
selector = expr ("," expr)*
expr     = key (op value)?
op       = "==" | "!=" | "in (" value ("," value)* ")" | "notin (" value ("," value)* ")"
```

Examples:

- `where: "role==worker"` (single equality)
- `where: "role==worker, zone!=eu-west"` (AND)
- `where: "tier in (gpu, fast)"`
- `where: "preempt"` (key exists, value ignored: shorthand for `key == any`)

We deliberately skip the more ornate parts of the k8s grammar (set-based with parentheses-as-precedence, regex, glob). The flat string form keeps the YAML/TOML readable and is enough for "give me a GPU host."

### Default-and-document calls flagged for operator review

- `spread: 1` is identical to singleton (no key). Allowed: the explicit form is more readable when a config is templated. **(operator review: should we reject `spread: 1` to keep one canonical form, or normalize at parse time?)** Default in this spec: normalize at parse time.
- `where:` matches zero hosts: the stack stays `Pending` with a `placement.no_eligible_hosts` event. **OPERATOR DECISION 2026-05-11 (locked, overrides earlier draft default):** option B (auto-place on first eligible enroll/label-change). The scheduler subscribes to host-enroll and heartbeat label-change events and re-evaluates `Pending` services; when a host becomes eligible the service is auto-placed onto it. The earlier draft default (A: stay Pending until manual redeploy) is overridden. Rationale: pending-forever was confusing in the brainstorm walkthrough; auto-placement matches the implicit promise of declarative placement verbs.

## TOML and YAML examples

### `spread`

```toml
# compose.toml (flat shape; every top-level table is a service)
[web]
image = "nginx:alpine"
spread = 3
```

```yaml
# compose.yaml (compose-compat shape; services wrapper)
services:
  web:
    image: nginx:alpine
    spread: 3
```

### `global`

```toml
[node-exporter]
image = "prom/node-exporter:latest"
global = true
```

```yaml
services:
  node-exporter:
    image: prom/node-exporter:latest
    global: true
```

### `on`

```toml
[postgres]
image = "postgres:16"
on = "alice"
```

```yaml
services:
  postgres:
    image: postgres:16
    on: alice
```

### `where` (modifier)

```toml
[gpu-worker]
image = "myorg/inference:v3"
spread = 4
where = "tier==gpu, role!=control"

[telegraf]
image = "telegraf:1.30"
global = true
where = "role==worker"

[prometheus]
image = "prom/prometheus:latest"
where = "role==monitoring"   # alone = singleton on a monitoring host
```

```yaml
services:
  gpu-worker:
    image: myorg/inference:v3
    spread: 4
    where: "tier==gpu, role!=control"
  telegraf:
    image: telegraf:1.30
    global: true
    where: "role==worker"
  prometheus:
    image: prom/prometheus:latest
    where: "role==monitoring"
```

## Swarm-compat translation table

Existing compose files using Docker Swarm `deploy.placement` keep working. The parser detects either shape, converts to the same internal `Placement`, and the rest of the pipeline doesn't care which surface was used.

| Swarm input | Translates to |
|---|---|
| `deploy.replicas: N` | `Placement::Spread { count: N, where: None }` |
| `deploy.mode: global` | `Placement::Global { where: None }` |
| `deploy.placement.constraints: ["node.role == worker"]` | `where: "role==worker"` (drops `node.` prefix) |
| `deploy.placement.constraints: ["node.hostname == alice"]` | `Placement::On { host: "alice", where: None }` |
| `deploy.placement.constraints: ["node.labels.tier == gpu"]` | `where: "tier==gpu"` (drops `node.labels.` prefix) |
| `deploy.placement.constraints: ["engine.labels.X == Y"]` | rejected with a clear error: engine labels are docker-engine-only and have no Isengard equivalent |

Mixing native verbs and `deploy.*` in the same service is a parse error: pick one. Mixing them in different services of the same compose is allowed but warned (the `isd deploy` output prints a "mixed placement style" notice).

## Compose parser changes

Touched crate: `isengard-agent` (`compose_reconciler.rs`).

### `DesiredService`

Add field:

```rust
pub struct DesiredService {
    // ... existing fields ...
    pub placement: Option<Placement>,
}
```

### New `Placement` enum (in `compose_reconciler.rs`, or split into its own `placement.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Default. One replica, scheduler picks any host that matches `where`.
    Singleton { selector: Option<LabelSelector> },
    /// N replicas. Prefer one-per-host. When fleet size < N, stack replicas
    /// onto already-used hosts (still trying to minimize concentration).
    Spread { count: u32, selector: Option<LabelSelector> },
    /// Exactly one replica per eligible host. Auto-place on new enrollees
    /// (after the next reconcile tick).
    Global { selector: Option<LabelSelector> },
    /// Pinned to a specific hostname. Never relocated.
    On { host: String, selector: Option<LabelSelector> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSelector {
    raw: String,             // canonical string (for round-trip)
    exprs: Vec<SelectorExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorExpr {
    Eq { key: String, value: String },
    Neq { key: String, value: String },
    In { key: String, values: Vec<String> },
    NotIn { key: String, values: Vec<String> },
    Exists { key: String },
}
```

### Parsing rules

In `parse_service`, after the existing fields:

1. Detect at most one of `spread`, `global`, `on` (else error: "conflicting placement verbs").
2. Detect `where` regardless and parse via `LabelSelector::parse` (else error with a span pointing to the bad expression).
3. Detect `deploy:` block; if present and any native verb is present in the same service, error.
4. If `deploy.replicas` / `deploy.mode` / `deploy.placement.constraints` is present, translate per the table above.
5. Set `svc.placement`.

The `parse_toml_with_native_placement_verbs_passes_through` test gets rewritten to assert the parsed `Placement` values match expectation. Add equivalent tests for the YAML form, the swarm-compat form, and the error cases (conflicting verbs, bad selector grammar, mixing native + swarm in one service).

## Agent label model

### Source of truth

Three sources, merged in this order (later wins):

1. `/etc/isengard/agent.toml` `[labels]` table:
   ```toml
   [labels]
   role = "worker"
   tier = "gpu"
   zone = "eu-west"
   ```
2. Environment variables matching `ISENGARD_LABEL_<KEY>=<VALUE>`. Useful for systemd: `Environment=ISENGARD_LABEL_ROLE=worker` in `iso-agent.service`.
3. Future: dynamic labels from runtime probes (e.g. detect GPU). NOT shipped in 0.14; placeholder design only.

Key rules:

- Keys: ASCII `[a-z0-9._-]+`, max 63 chars, lowercased on ingest. Conflicting case-insensitive duplicates rejected.
- Values: any UTF-8 except commas (for selector grammar safety) and `=`.
- Reserved keys: `host`, `hostname` (use existing host identity), `id`. Reserved-key writes warned and dropped, not errored, to avoid bricking an agent on a misconfig.

### Heartbeat shape

Extend `Heartbeat` proto:

```proto
message Heartbeat {
  uint64 ts_ms = 1;
  repeated StackInfo stacks = 2;
  repeated ServiceInfo services = 3;
  string runtime_backend = 4;
  // Phase 0.14: agent labels for placement selectors. Empty map from
  // older agents is treated as "no labels" (matches any unfiltered
  // placement, doesn't match any `where:`-constrained one).
  map<string, string> labels = 5;
}
```

Additive; older agents are still accepted. Controller stores the labels in a new `agent_labels` table (keyed by `host_id`, replaced on each heartbeat) so the scheduler can read them without a heartbeat round-trip.

## Scheduler design

### Where it lives

New module `crates/isengard-controller/src/scheduler/`:

```
scheduler/
  mod.rs         pub Scheduler struct, public API
  desired.rs     parse Placement -> set of (service, replica_index) demands
  eligible.rs    host eligibility under selector
  assign.rs      bin-packing / spread logic
  reconcile.rs   reconcile loop: compare desired vs current, emit ops
  events.rs      placement.* event helpers
```

Owned by `Controller::start()` alongside the existing orchestrator. Not a separate crate: the scheduler reads from `Inventory`, writes via the existing `HostAction` queue, and uses `EventBus` for placement events. Lifting it to its own crate is reasonable later but adds a build edge today for little gain.

### State model

In-memory (rebuilt on controller start from the persisted `placements` table):

```rust
pub struct SchedulerState {
    // service_id -> all current placements for that service
    placements: HashMap<ServiceId, Vec<PlacementAssignment>>,
    // host_id -> live label set (replaced each heartbeat)
    labels: HashMap<HostId, BTreeMap<String, String>>,
    // host_id -> "healthy" flag derived from last_seen_at + Pending grace
    health: HashMap<HostId, HostHealth>,
}

pub struct PlacementAssignment {
    pub service_id: ServiceId,
    pub host_id: HostId,
    pub replica_index: u32,
    pub assigned_at: DateTime<Utc>,
    pub state: PlacementState,    // Pending, Active, Draining, Failed
}

pub enum HostHealth {
    Healthy,            // last_seen < heartbeat_interval * 2
    Disconnected { since: DateTime<Utc> },  // last_seen > threshold
    Flapping { since: DateTime<Utc>, flip_count: u32 },
}
```

### Persisted state

New migration `0027_placements.sql`:

```sql
CREATE TABLE placements (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    service_id      INTEGER NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    host_id         BLOB    NOT NULL REFERENCES hosts(id)    ON DELETE CASCADE,
    replica_index   INTEGER NOT NULL,
    state           TEXT    NOT NULL CHECK(state IN ('pending','active','draining','failed')),
    assigned_at     TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_event      TEXT,
    UNIQUE(service_id, replica_index),
    UNIQUE(service_id, host_id, replica_index)
);
CREATE INDEX idx_placements_service ON placements(service_id);
CREATE INDEX idx_placements_host    ON placements(host_id);

CREATE TABLE agent_labels (
    host_id   BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (host_id, key)
);
```

Migrating existing services: any service row without a corresponding `placements` row is treated as "already placed on host_id, no Placement model needed" and a synthetic `Singleton { selector: None }` is inferred (with `replica_index = 0`). Verb-driven re-placement is opt-in: an operator re-deploys to engage the scheduler.

**(operator review: A: backfill `placements` rows for every existing service at migration time with state='active' and `replica_index=0`, so the scheduler sees them immediately; B: lazy-create on first scheduler tick that touches the service.)** Spec default: A. Backfill at migration is one statement and avoids a class of "scheduler thinks the service is unplaced" bugs.

### Trigger events

Reconcile is event-driven AND timer-driven. The timer is a safety net.

| Event | Trigger | Action |
|---|---|---|
| Service desired-state change (deploy / update) | `compose_broker` reports a new compose | Reconcile the affected services |
| Agent enroll | `EnrollmentService::redeem` | Reconcile services with `Placement::Global` or unsatisfied `Spread` |
| Agent disconnect_long (>60s) | `DisconnectMonitor` event | Mark host disconnected; reconcile any service that has a placement there |
| Agent rejoin | First heartbeat after disconnect | Reconcile (may revert a freshly placed-elsewhere replica? See conflict resolution below) |
| Label change | First heartbeat where `labels` map differs from cached | Reconcile services with selectors |
| Reconcile timer | Every 15s | Reconcile every service. Cheap full sweep against in-memory state |

The interval is short on purpose: scheduler ticks are cheap (no IO except DB reads), and the 15s safety net catches lagging events without operators noticing.

### Reconcile loop (pseudocode)

```rust
async fn reconcile_service(&self, service_id: ServiceId) -> Result<()> {
    let desired = self.parse_placement(service_id).await?;
    let current = self.placements.get(&service_id).cloned().unwrap_or_default();
    let healthy = self.eligible_hosts(&desired.selector);

    let want: Vec<(HostId, u32)> = match desired.placement {
        Placement::Singleton { .. } => {
            if current.iter().any(|p| healthy.contains(&p.host_id) && p.state == Active) {
                current.iter().filter(|p| p.state == Active).map(|p| (p.host_id, 0)).collect()
            } else {
                let h = pick_least_loaded(&healthy)?;
                vec![(h, 0)]
            }
        }
        Placement::Spread { count, .. } => {
            self.spread_assign(count, &healthy, &current)
        }
        Placement::Global { .. } => {
            healthy.iter().enumerate().map(|(i, h)| (*h, i as u32)).collect()
        }
        Placement::On { host, .. } => {
            // healthy is already filtered by selector
            if let Some(h) = self.host_by_name(&host).await? {
                if healthy.contains(&h) {
                    vec![(h, 0)]
                } else {
                    self.emit_event("placement.unavailable", ...);
                    vec![]   // do NOT relocate
                }
            } else {
                self.emit_event("placement.unknown_host", ...);
                vec![]
            }
        }
    };

    let to_create = want.iter().filter(|w| !current.contains(w)).collect();
    let to_drain  = current.iter().filter(|c| !want.iter().any(|w| w == &(c.host_id, c.replica_index))).collect();

    for (host, idx) in to_create { self.dispatch_apply(service_id, host, idx).await?; }
    for c                 in to_drain  { self.dispatch_drain(c).await?; }

    Ok(())
}
```

`spread_assign` implements: walk `healthy` host list sorted by `(current_load, hostname)`, assign one replica per host until `count` is reached. If `healthy.len() < count`, second pass round-robins onto already-used hosts.

### Conflict resolution

**Two stacks declare `on: alice`.** Allowed and expected. The scheduler treats each service independently; both place onto alice. Same as today's behavior with two services pinned via host hint.

**A stack declares `on: alice` and alice doesn't exist (typo).** Placement stays `Pending`. Event `placement.unknown_host` emits once per service per missing-host occurrence (deduped via in-memory set). Operator fixes the compose or enrolls the host.

**Spread placement: tie-break.** Hosts sorted by `(active_placement_count_for_this_service, total_active_placement_count, hostname_alphabetical)`. Deterministic; an operator can predict where a fresh replica lands.

**Disconnect grace: replica lands on B, then host A returns.** A's old replica gets a `placement.duplicate` event and is drained (per the scheduler's view, the freshly placed replica on B is authoritative). The operator sees "replica reclaimed from A". This is the safer default vs. preferring A. **(operator review: A: drain A's replica when A returns; B: drain B's replica and prefer the original host.)** Spec default: A. Preferring the new host avoids ping-pong if A flaps.

**Global placement on a flapping host.** When a host flaps (disconnect/reconnect twice within 5 minutes), it's marked `Flapping` and excluded from new placements but EXISTING placements on it stay. Mark as healthy again after 5 minutes of stable heartbeats.

### Health gating

A host is eligible for new placements only when `HostHealth::Healthy`. Existing placements on `Disconnected` hosts stay in the `Draining` state for the grace period (default 60s) before being reassigned. `Flapping` hosts keep what they have but get no new work.

Heartbeat fields used:
- `last_seen_at` from the `hosts` row.
- An in-scheduler `flip_count` incremented every time a host transitions Healthy -> Disconnected within a 5-minute window.

Threshold default: 60s. Configurable via controller flag `--placement-grace-secs` (also surfaced in `isd settings`). Open question: **(operator review: should the grace period be per-service via compose `placement.grace_secs:`, or fleet-wide only?)** Spec default: fleet-wide only in 0.14; per-service in 0.15+ if anyone asks. Per-service grace is rarely used in practice.

### Failure modes

| Mode | Behavior |
|---|---|
| `spread: N` but `eligible_hosts.len() < N` | Place onto all available, emit `placement.degraded { wanted: N, got: M, missing: N-M }` once. Service state: `DegradedPlacement` (new variant on the service state machine). Banner in dashboard later. |
| `where:` matches zero hosts | Service stays `Pending` and emits `placement.no_eligible_hosts { selector, fleet_size }` once. Scheduler watches host-enroll + label-change events and auto-places on the first eligible host (operator decision 2026-05-11). |
| Pinned host disappears | Service becomes `Unavailable` (new state variant). Emit `placement.host_gone { host }`. No relocation. Operator changes the compose to a different host or restores the original. |
| Selector grammar error | Hard parse error at `isd deploy`. Stack is not deployed. |
| Mixed verb+swarm in same service | Hard parse error. |
| Placement assignment fails to dispatch (gRPC down to host A) | Existing per-host retry behavior in `HostAction` queue handles it; scheduler treats the placement as Pending until success. |

## isd CLI additions

Two new subcommands under `isd placement`:

```
isd placement show <stack>                    # grid of service x host x replica
isd placement explain <stack>/<service>       # why each host was selected or rejected
```

### `placement show` output

```
$ isd placement show monitoring
SERVICE         REPLICA  HOST       STATE     ASSIGNED
prometheus      0        alice      active    2026-05-11 09:14:02Z
node-exporter   0        alice      active    2026-05-11 09:14:02Z
node-exporter   1        bob        active    2026-05-11 09:14:02Z
node-exporter   2        carol      active    2026-05-11 09:14:02Z
gpu-worker      0        bob        active    2026-05-11 09:14:02Z
gpu-worker      1        carol      draining  2026-05-11 09:14:02Z
gpu-worker      1        dave       pending   2026-05-11 09:15:18Z

stack: monitoring (fleet=lab)
desired:
  prometheus     singleton, where: "role==monitoring"
  node-exporter  global
  gpu-worker     spread: 3, where: "tier==gpu"
```

### `placement explain` output

```
$ isd placement explain monitoring/gpu-worker
service: monitoring/gpu-worker
placement: spread: 3, where: "tier==gpu"

ELIGIBLE HOSTS (matched selector):
  bob     labels: tier=gpu role=worker   healthy
  carol   labels: tier=gpu role=worker   draining (last_seen 92s ago, grace=60s)
  dave    labels: tier=gpu role=worker   healthy
  eve     labels: tier=gpu role=control  EXCLUDED: where requires tier=gpu but role!=control? no, role does not appear in selector
  alice   labels: tier=cpu role=control  EXCLUDED: tier=cpu != tier=gpu

CURRENT PLACEMENT:
  replica 0 -> bob   (active since 2026-05-11 09:14:02Z)
  replica 1 -> carol (DRAINING since 2026-05-11 09:14:50Z; grace expires 2026-05-11 09:15:50Z)
  replica 1 -> dave  (PENDING since 2026-05-11 09:15:18Z)
  replica 2 -> ?     (UNFILLED; no fourth eligible host)
```

Implementation: both subcommands hit a new gRPC method `PlacementShow(StackId) -> PlacementSnapshot` on the controller plugin RPC. The "explain" view does the eligibility walk in the controller and ships a structured per-host reasoning record so the client just renders.

## Migration

- Existing services keep their `placements` rows backfilled at migration time with `state='active'` and `replica_index=0`. The scheduler sees them as singleton placements.
- Compose files without any of the new verbs are unchanged: `Placement::Singleton { selector: None }` is the implicit default.
- The 60s default grace period matches the existing `disconnect_long` 4h threshold for emitting an event, but is a separate timer with a separate purpose. Document both.
- Older agents (without the labels heartbeat field) keep working; they're treated as having no labels. Any service with a `where:` clause won't be placed on them.

## Risks

- **Scheduler is stateful in-memory + persisted.** Recovery on controller restart: rebuild `SchedulerState` from `placements` + `agent_labels` + `hosts` rows. The reconcile loop on first tick should produce identical decisions to what was in flight pre-restart. Test: kill the controller mid-reconcile (after a `to_create` dispatch, before the placement row writes), restart, verify the placement row is created without a duplicate dispatch. This is the trickiest correctness property in the design.
- **Reconcile-loop chatter.** A label change on every heartbeat from one noisy agent could cause a full sweep every heartbeat. Mitigation: hash the agent's labels and skip the reconcile when the hash hasn't changed. Add a metric `scheduler.reconciles_per_minute` to catch regressions.
- **Two controllers writing to the same fleet.** Out of scope (each fleet has exactly one controller per the brainstorm's Model A) but worth a guard rail: the controller writes its own `controller_id` into the `placements.last_event` JSON. A future multi-controller setup would catch fights early.
- **Drift between agent and scheduler.** The agent runs its own compose reconciler today. If the operator hand-edits `/etc/isengard/stacks/<stack>/compose.yaml` on a host, the agent will reconcile to that state locally. The scheduler's view of "placement on host X" doesn't know the on-disk compose changed. 0.14 doesn't fight that: the scheduler operates on the controller's compose, and the host-local copy is a cache.
- **The `where:` selector is a tiny language.** Bad parse messages here are a pure UX cost. Invest in a small but readable parser with `nom` or `peg`; don't lean on a custom regex.

### Risks flagged for explicit operator review

Resolved 2026-05-11 (locked):

- **Per-service vs fleet-wide grace period** -> fleet-wide only in 0.14; per-service deferred to 0.15+.
- **`where:` zero-match behavior: stay Pending vs auto-deploy on enroll** -> **auto-place on first eligible enroll/label-change** (overrides earlier "stay Pending" draft).
- **Disconnect grace tie-break: prefer new host vs original host on rejoin** -> prefer new host (drain the original on return).
- **`spread: 1` form: reject or normalize** -> normalize to `Singleton` at parse time.
- **Backfill of `placements` rows at migration** -> backfill rows for every existing service (state='active', replica_index=0).

## Out of scope, explicitly

- Multi-controller failover. One controller per fleet; if the controller dies, scheduling pauses. Existing agents continue running their last-known assignments.
- Resource-aware placement. No CPU/memory bin-packing.
- Affinity rules between services within a stack.
- A "drain a host" verb at the scheduler level. `isd fleet stop <host>` already cordons the host; the scheduler will see it as ineligible.
- Web UI for placement. CLI only in 0.14; dashboard tile lands when dashboard work resumes.

## Open questions for implementation

- **`isd diff` integration.** The existing `isd diff` shows a per-host compose-style diff. Once the scheduler is authoritative, `isd diff` should grow a `--placement` mode that shows desired vs current placement assignment. Defer to a follow-up commit in the same plan.
- **Event bus retention for placement events.** Today the journal stores everything; placement events will be noisy under churn. Decision deferred to 0.15: tag placement events with a `placement` category, add a retention policy later.
- **Conflict between a `spread: 3` service and an `on: alice`-pinned replica that previously ran the same service.** Treat the `on:` form as a hard override: it gets replica 0; spread fills the remainder. **(operator review: should a service be allowed to use BOTH `on:` for one anchor replica AND `spread:` for the rest? Spec default: NO. Pick one verb. The complexity isn't worth it for 0.14.)**
