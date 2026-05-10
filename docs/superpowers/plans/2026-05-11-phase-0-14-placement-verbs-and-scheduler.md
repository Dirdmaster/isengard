# Phase 0.14 Placement Verbs + Scheduler: Implementation Plan

> Spec: [`2026-05-11-phase-0-14-placement-verbs-and-scheduler-design.md`](../specs/2026-05-11-phase-0-14-placement-verbs-and-scheduler-design.md). Branch: `phase/0-14-scheduler`.

## Scope

Wire native placement verbs (`spread`, `global`, `on`, `where`) through the parser, ship a scheduler in `isengard-controller`, add agent labels to the heartbeat, and add `isd placement show/explain`. Defaults per the spec. Existing services keep working without re-deploy.

Out of scope: autoscaling, gang scheduling, affinity, resource-aware placement, dashboard tiles, multi-controller failover.

## Dev environment

Same as Phase 0.13: OrbStack VM `wisp` (Ubuntu 24.04.4, arm64). Local-edit + remote-test loop via the bind-mount on `/Users/dirdmaster/...`. Two-host scheduling tests require a second VM `iso-bob`; existing `iso-test` reused as `iso-alice`. Spin `iso-bob` once before Step 8.

## Files touched

| File | Change |
| --- | --- |
| `crates/isengard-agent/src/compose_reconciler.rs` | add `Placement` enum, `LabelSelector`, parse logic, update `DesiredService` |
| `crates/isengard-agent/src/labels.rs` | extend: load from `/etc/isengard/agent.toml` + `ISENGARD_LABEL_*` env, attach to heartbeat |
| `crates/isengard-agent/src/sync.rs` | populate heartbeat `labels` field on each tick |
| `crates/isengard-proto/proto/isengard.v1.proto` | add `map<string, string> labels = 5` to `Heartbeat` |
| `crates/isengard-storage/migrations/0027_placements.sql` | new: `placements`, `agent_labels` tables; backfill existing services |
| `crates/isengard-storage/src/placements.rs` | new DAO: list/upsert/delete placement rows, list labels by host |
| `crates/isengard-storage/src/lib.rs` | re-export the new DAO |
| `crates/isengard-controller/src/scheduler/mod.rs` | new Scheduler struct, public API |
| `crates/isengard-controller/src/scheduler/desired.rs` | parse `Placement` -> assignment demand list |
| `crates/isengard-controller/src/scheduler/eligible.rs` | host eligibility under selector |
| `crates/isengard-controller/src/scheduler/assign.rs` | spread / global / on assignment math |
| `crates/isengard-controller/src/scheduler/reconcile.rs` | reconcile loop, event handlers |
| `crates/isengard-controller/src/scheduler/events.rs` | placement.* event helpers |
| `crates/isengard-controller/src/service.rs` | persist `labels` from heartbeat; trigger scheduler reconcile |
| `crates/isengard-controller/src/disconnect_monitor.rs` | emit a synchronous-ish trigger into the scheduler on long disconnect |
| `crates/isengard-controller/src/enrollment.rs` | trigger scheduler reconcile of `Global`/`Spread` services on enroll |
| `crates/isengard-controller/src/lib.rs` | wire `Scheduler` into `Controller::start()` |
| `crates/isengard-controller/src/plugin_host.rs` | new gRPC method `placement_show` + `placement_explain` (internal RPC) |
| `crates/isd/src/main.rs` | new `placement` subcommand group |
| `crates/isd/src/placement_cmd.rs` | new: `show`, `explain` printers |
| `docs/PLACEMENT.md` | new operator-facing reference for verbs + selectors |
| `install/agent.toml.example` | add `[labels]` table with role/tier examples |

## Steps

Each step ends in a commit. Branch is `phase/0-14-scheduler`. Subagent dispatch model: Opus implementers per task; Sonnet code reviewer at every checkpoint where the spec calls one out.

### 1. Heartbeat labels: proto + storage + agent send

Add `map<string, string> labels = 5;` to `Heartbeat` in `isengard.v1.proto`. Regenerate proto.

Migration `0027_placements.sql` lands in this commit (only the `agent_labels` portion is used yet; the `placements` table is created but unused until Step 5). Provides:

```sql
CREATE TABLE agent_labels (host_id BLOB, key TEXT, value TEXT, PRIMARY KEY (host_id, key));
CREATE TABLE placements  (...);   -- per spec
INSERT INTO placements (service_id, host_id, replica_index, state, assigned_at)
  SELECT id, host_id, 0, 'active', last_seen_at FROM services;
```

Storage DAO `placements.rs`:

```rust
pub async fn list_labels(host_id: HostId) -> Result<BTreeMap<String, String>>;
pub async fn replace_labels(host_id: HostId, labels: BTreeMap<String, String>) -> Result<()>;
pub async fn list_placements(service_id: ServiceId) -> Result<Vec<PlacementRow>>;
pub async fn list_placements_by_host(host_id: HostId) -> Result<Vec<PlacementRow>>;
pub async fn upsert_placement(p: PlacementRow) -> Result<()>;
pub async fn delete_placement(service_id: ServiceId, host_id: HostId, replica_index: u32) -> Result<()>;
```

Agent side: extend `crates/isengard-agent/src/labels.rs` (currently container-labels only; add an `agent_labels` submodule or split into a new file). Loader:

```rust
pub fn load_agent_labels() -> BTreeMap<String, String> {
    let from_toml = load_from_agent_toml().unwrap_or_default();
    let from_env  = load_from_env();
    merge(from_toml, from_env)   // env wins
}
```

`sync.rs`: on each heartbeat assembly, populate the new field. Reused result cached for the agent lifetime; re-read on SIGHUP (future feature; for 0.14 a single load at start is enough).

Tests:

- Storage unit: `agent_labels` round-trip + replace.
- Storage unit: `placements` backfill from existing services on migration. Empty services -> zero placements.
- Agent unit: TOML loader with valid + invalid keys (case folding, reserved key dropped).
- Agent unit: env loader: `ISENGARD_LABEL_ROLE=worker` parsed to `role=worker`.
- Agent unit: merge order (env wins).
- Integration: agent connects, sends one heartbeat, controller `agent_labels` row exists for the host.

Commit: `feat(proto+storage): heartbeat labels and placement migration`

Subagent: 1 Opus implementer. Review checkpoint after commit: confirm the migration is reversible-safe (one new table + one ALTER-like INSERT; no destructive op).

### 2. Compose parser: `Placement` and `LabelSelector`

`compose_reconciler.rs` gains a new `Placement` enum and `LabelSelector` struct per the spec. Add `placement: Option<Placement>` to `DesiredService`. Parser changes:

- After existing fields in `parse_service`, detect any of `spread`, `global`, `on`. At most one allowed.
- Detect `where`. Parse via `LabelSelector::parse` (new module `compose_reconciler/selector.rs` or inline).
- Detect `deploy:` block. Translate per the spec's swarm-compat table. Error if mixed with native verbs.

`LabelSelector::parse(s: &str) -> Result<LabelSelector>`:

- Split on `,` at top level (no nested parens for the 0.14 grammar).
- For each expr, regex match against `key (==|!=|in \(...\)|notin \(...\)) value` or `key` alone (Exists).
- Validate key chars `[a-z0-9._-]+`, max 63 chars.
- Build `SelectorExpr`. Return canonical `raw` string from re-emit (for round-trip in `placement explain`).

Tests (add to `compose_reconciler.rs` test module, ~25 cases):

- `spread: 3` parsed as `Spread { count: 3, selector: None }`.
- `global: true` parsed as `Global { selector: None }`.
- `on: "alice"` parsed as `On { host: "alice", selector: None }`.
- `where: "role==worker"` alone parsed as `Singleton { selector: Some(...) }`.
- `spread: 3` + `where: "tier==gpu"` parsed as `Spread { count: 3, selector: Some(...) }`.
- `spread: 3` + `global: true` -> error "conflicting placement verbs".
- `deploy.replicas: 3` translated to `Spread { count: 3 }`.
- `deploy.placement.constraints: ["node.role == worker"]` translated to `where: "role==worker"`.
- `deploy.placement.constraints: ["node.hostname == alice"]` translated to `On { host: "alice" }`.
- `deploy.placement.constraints: ["engine.labels.X == Y"]` -> error.
- `spread: 1` normalized to `Singleton`.
- `where: "tier in (gpu, fast)"` parsed.
- `where: "preempt"` parsed as `Exists`.
- `where: "role==worker, zone!=eu-west"` parsed (AND).
- Bad selector (`==value`) -> error with span.
- Native verbs + `deploy:` in same service -> error.
- TOML form for each above (extends the existing `parse_toml_with_native_placement_verbs_passes_through`).
- YAML form for each above.
- YAML/TOML equivalence (extend the existing equivalence test).

Rewrite (not extend) the existing `parse_toml_with_native_placement_verbs_passes_through` test to assert the parsed `Placement` values.

Commit: `feat(compose): parse placement verbs and label selectors`

Subagent: 1 Opus implementer. Review checkpoint after commit. The selector parser is the most user-visible surface in the phase; sloppy error messages will haunt every operator.

### 3. Scheduler skeleton

Create `scheduler/` module under `isengard-controller`. Empty-but-typed:

```rust
pub struct Scheduler {
    inventory: Arc<Inventory>,
    placements_dao: Arc<Placements>,   // step 1
    bus: Arc<EventBus>,
    grace: Duration,
    // populated from disk on start
    state: Arc<Mutex<SchedulerState>>,
}

impl Scheduler {
    pub async fn new(inventory: Arc<Inventory>, placements_dao: Arc<Placements>, bus: Arc<EventBus>, grace_secs: u64) -> Result<Self>;
    pub fn start(self: Arc<Self>) -> JoinHandle<()>;   // spawns the reconcile loop
    pub async fn reconcile_service(&self, service_id: ServiceId) -> Result<()>;   // stub: log + return Ok
    pub async fn reconcile_all(&self) -> Result<()>;                              // stub
    pub async fn on_heartbeat_labels(&self, host_id: HostId, new_labels: BTreeMap<String, String>);  // stub
    pub async fn on_host_enroll(&self, host_id: HostId);                          // stub
    pub async fn on_host_disconnect_long(&self, host_id: HostId);                 // stub
    pub async fn snapshot(&self, stack_id: StackId) -> Result<PlacementSnapshot>; // stub: returns empty
}
```

Wire into `Controller::start()` in `lib.rs`: `let scheduler = Arc::new(Scheduler::new(...).await?);` next to the orchestrator, store on `Controller` struct, spawn `scheduler.clone().start()`. Reconcile loop ticks every 15s and currently does nothing.

Tests:

- Skeleton compiles.
- `Scheduler::new` rebuilds in-memory `placements` map from the DAO (using backfilled rows from migration).
- Reconcile timer fires at the configured interval (via a test-mode `tick_now()` hook).

Commit: `feat(scheduler): controller-side skeleton`

Subagent: 1 Opus implementer. Review checkpoint after commit: confirm the wiring into `Controller::start()` survives the existing controller boot integration tests. The reconcile loop should be inert at this step.

### 4. Eligibility + selector matching

`scheduler/eligible.rs`:

```rust
pub fn match_selector(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool;
pub fn eligible_hosts(
    hosts: &[Host],
    labels_by_host: &HashMap<HostId, BTreeMap<String, String>>,
    health_by_host: &HashMap<HostId, HostHealth>,
    selector: Option<&LabelSelector>,
) -> Vec<HostId>;
```

`match_selector` walks each `SelectorExpr` and returns true iff every clause matches (`Eq`, `Neq`, `In`, `NotIn`, `Exists`).

`eligible_hosts` keeps only Healthy hosts whose labels satisfy the selector. Flapping and Disconnected hosts are excluded.

Tests:

- `Eq` matches.
- `Neq` matches (key present + value differs; key absent treated as match per k8s semantics).
- `In` matches.
- `NotIn` matches.
- `Exists` matches.
- `None` selector matches any healthy host.
- Conjunction of three exprs all match.
- One clause fails -> overall fails.
- Flapping host excluded.
- Disconnected host excluded.

Commit: `feat(scheduler): selector matching and host eligibility`

Subagent: 1 Opus implementer.

### 5. Spread / global / on assignment math

`scheduler/assign.rs`:

```rust
pub fn assign_singleton(eligible: &[HostId], current: &[PlacementRow]) -> Vec<(HostId, u32)>;
pub fn assign_spread(count: u32, eligible: &[HostId], current: &[PlacementRow], load: &HashMap<HostId, u32>) -> Vec<(HostId, u32)>;
pub fn assign_global(eligible: &[HostId]) -> Vec<(HostId, u32)>;
pub fn assign_on(host: HostId, eligible_contains_host: bool) -> Vec<(HostId, u32)>;
```

Spread algorithm:

1. Sort eligible by `(active_placement_count_for_this_service, total_active_placement_count, hostname_alpha)`.
2. First pass: assign one replica to each of the first `min(count, eligible.len())` hosts.
3. If `count > eligible.len()`, second pass: round-robin onto already-assigned hosts until `count` is reached, incrementing `replica_index`.

Global: each eligible host gets exactly one replica, indices `0..eligible.len()`.

On: returns `[(host, 0)]` if eligible-contains-host, else `[]`.

Tests (table-driven):

- spread 3 / 3 eligible -> 3 distinct hosts.
- spread 3 / 2 eligible -> alice gets [0,2], bob gets [1] (round-robin).
- spread 1 / 3 eligible -> 1 host.
- spread 3 / 0 eligible -> []. Emit `placement.degraded`.
- global / 3 eligible -> 3 placements.
- global / 0 eligible -> [].
- on alice / alice eligible -> [(alice, 0)].
- on alice / alice excluded -> [].

Commit: `feat(scheduler): spread/global/on assignment`

Subagent: 1 Opus implementer.

### 6. Reconcile loop wired to dispatch

`scheduler/reconcile.rs`: implement `reconcile_service` per the spec's pseudocode.

- Read service's compose from inventory.
- Determine `Placement` (from parsed compose).
- Compute eligible hosts.
- Compute want set via assign.rs.
- Diff against current placement rows.
- Dispatch creates (queue a `HostAction::ApplyCompose` per host, like the existing per-host apply path).
- Dispatch drains (queue a `HostAction::StopService` for the replica being removed).
- Persist placement row state transitions.
- Emit placement events (placement.created, placement.removed, placement.degraded, etc.).

`reconcile_all` walks every service and calls `reconcile_service`.

Event triggers:

- Reconcile timer: every 15s call `reconcile_all`.
- `on_heartbeat_labels`: replace cached labels, then `reconcile_all` if hash changed.
- `on_host_enroll`: reconcile services with `Global` or unsatisfied `Spread` (filter via service.placement; skip Singleton/On).
- `on_host_disconnect_long`: mark host disconnected, schedule a `reconcile_service` for each service the host had after `grace_secs`.

Tests (controller integration, in-memory inventory, mocked dispatch):

- `spread: 3` on 3-host fleet -> 3 placements, one per host.
- `spread: 3` on 2-host fleet -> 3 placements, replica_index 0 and 2 on alice, 1 on bob.
- `global: true` on enrollment of carol after deploy -> carol gets a placement.
- `on: alice` -> single placement on alice.
- `on: alice` when alice is missing -> no placement, `placement.unknown_host` event.
- `on: alice` when alice disconnects -> placement stays, `placement.host_gone` event, NO relocation.
- `spread: 3` when alice disconnects past grace -> replica drained, redispatched to dave (if eligible).
- `where: "role==worker"` with no matching hosts -> stays Pending, `placement.no_eligible_hosts` event.
- Controller restart mid-reconcile: in-memory state rebuilt from `placements` table; no duplicate dispatch.

Commit: `feat(scheduler): reconcile loop and host action dispatch`

Subagent: 1 Opus implementer. Review checkpoint after commit. The reconcile loop is the highest-stakes piece; ask for an explicit walkthrough.

### 7. Controller integration: hook scheduler into existing pipeline

Touch `service.rs`, `enrollment.rs`, `disconnect_monitor.rs`, `compose_broker.rs` to call into the scheduler:

- `service.rs`: on heartbeat receive, after `process_heartbeat_services`, call `scheduler.on_heartbeat_labels(host_id, labels)` (per-heartbeat; cheap with the hash skip).
- `enrollment.rs`: after a successful `redeem`, call `scheduler.on_host_enroll(host_id)`.
- `disconnect_monitor.rs`: on `agent.disconnect_long`, call `scheduler.on_host_disconnect_long(host_id)`.
- `compose_broker.rs` (or the equivalent path that handles a new compose YAML write): after the new compose lands, call `scheduler.reconcile_service` for each affected service.

Tests:

- End-to-end via an existing controller integration harness if one exists; else a focused test that brings up a controller in-process and exercises one full deploy -> placement assignment round.

Commit: `feat(controller): wire scheduler into heartbeat/enroll/disconnect/compose`

Subagent: 1 Opus implementer.

### 8. Multi-host scheduling integration test

End-to-end test in `crates/isengard-controller/tests/scheduler_multi_host.rs`. Uses the in-process test harness (no real network, no real agents) but spins multiple `MockAgent` workers to simulate two hosts. The mocks send heartbeats, declare labels, and accept `HostAction`s.

Scenarios:

1. Deploy `spread: 2` across alice + bob: both get a replica.
2. Deploy `global: true`: both get a replica. Enroll carol. After next reconcile, carol gets a replica.
3. Deploy `spread: 3` with 2 hosts: alice and bob get replicas 0 and 1; one of them gets replica 2 round-robin.
4. Deploy `spread: 3` with 3 hosts; disconnect bob for 90s; replica reschedules to a remaining host. Bob reconnects; the freshly placed replica stays (default policy in spec), bob's old replica drained.
5. Deploy `where: "role==worker"` when no host has `role=worker`: stays Pending. Then update alice's labels via mock heartbeat; reconcile picks up. **(operator review confirmed: this scenario verifies the "do not auto-deploy on enroll/label change" default the spec calls out: if the operator picks B during review, this test changes.)**
6. Pinned `on: alice` -> alice gets it; alice disconnects -> service Unavailable; NO relocation.

Optional manual test on real OrbStack VMs (iso-alice + iso-bob): document the steps in `docs/PLACEMENT.md`'s test plan, but don't gate the commit on it.

Commit: `test(scheduler): multi-host integration scenarios`

Subagent: 1 Opus implementer.

### 9. isd placement CLI

`crates/isd/src/placement_cmd.rs` + dispatch from `main.rs`:

```
isd placement show <stack>
isd placement explain <stack>/<service>
```

Both hit a new gRPC method on the controller's plugin RPC: `PlacementShow(stack_id) -> PlacementSnapshot` and `PlacementExplain(stack_id, service_name) -> PlacementExplanation`. Proto messages added in `isengard.v1.proto`.

`PlacementSnapshot` includes per-service-per-replica current state + the parsed Placement spec. `PlacementExplanation` includes the per-host eligibility walk with reasoning strings.

Pretty printer follows the spec's example output, in plain ASCII (no color). Add `--json` flag for scripted use.

Tests:

- `placement show` against the integration controller from step 8: output matches a recorded fixture.
- `placement explain` against the same: output matches a fixture.
- `--json` mode emits valid JSON parseable by `serde_json::from_str::<PlacementSnapshot>`.

Commit: `feat(isd): placement show and explain subcommands`

Subagent: 1 Opus implementer.

### 10. Docs and example agent.toml

`docs/PLACEMENT.md`: operator-facing reference.

- Verb syntax (TOML + YAML for each, copy-pastable).
- Selector grammar with examples.
- Swarm-compat translation table.
- Failure-mode reference (degraded, Pending, Unavailable).
- Sample two-host walkthrough using OrbStack.

`install/agent.toml.example`: add a commented `[labels]` block.

```toml
# /etc/isengard/agent.toml
# ...existing fields...

# Phase 0.14: agent labels for placement selectors.
# Override or extend via `ISENGARD_LABEL_*` env vars in the systemd unit.
# [labels]
# role = "worker"
# tier = "gpu"
# zone = "eu-west"
```

`crates/isengard-agent/src/labels.rs` doc comment links to PLACEMENT.md.

Commit: `docs(placement): operator reference + example agent.toml`

Subagent: 1 Opus implementer.

### 11. Self-review and gate sweep

Run full gate set on the wisp VM:

- `cargo build --workspace --release`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo deny check`
- Run the multi-host integration test under `RUST_LOG=isengard_controller::scheduler=trace` and skim the trace output for surprises.

Fix anything that surfaces. Loop back to the relevant earlier step's tests and rerun.

Commit (if changes): `fix(scheduler): gate sweep nits`

Subagent: main session.

### 12. PR

Open a PR `phase/0-14-scheduler` -> `next`. Title: `feat: phase 0.14 placement verbs and scheduler`. Body links the spec, summarizes the verb set, calls out the five "operator review" defaults the spec flagged, and includes a one-screen `isd placement show` example.

Hold for operator review before merging.

Commit: PR open (not a code commit).

Subagent: main session.

## Validation

- All step-level test suites green from the wisp VM.
- `cargo test -p isengard-controller scheduler` passes deterministically (run three times back-to-back to catch flake).
- `cargo test -p isd placement` passes.
- `isd placement show monitoring` and `isd placement explain monitoring/gpu-worker` produce the documented output shape against the test harness.
- Existing services on a pre-0.14 controller, after migrating to 0.14, retain placement (visible via `isd placement show`); no churn during the migration.
- Older agents (no labels in heartbeat) keep working; the `agent_labels` row stays empty for them.

## Review checkpoints

- **After step 2** (parser): operator skims the selector grammar errors. Sloppy error messages here cost every operator interaction.
- **After step 3** (scheduler skeleton): confirm controller boot integration still green. Nothing else moves until this is solid.
- **After step 6** (reconcile loop): walkthrough requested. This is the highest-stakes piece. Specifically check the "controller restarts mid-reconcile" recovery test.
- **Before step 12** (PR): full demo of `placement show` / `explain` against a real two-host setup, OR a recorded fixture review if the second VM isn't ready.

## Risks

- **Spread tie-break.** A surprising placement decision is a debugging tax. The deterministic sort + the `placement explain` view are the operator's escape hatch.
- **Controller restart recovery.** Step 6's test covers it; if it flakes, stop and re-design the persistence boundary before continuing.
- **Reconcile chatter.** The label-hash skip is load-bearing. If a regression makes it always-fire, the controller will sweep every 5 seconds under churn. Add a metric in step 7 (`scheduler.reconciles_per_minute`) and alert if >10/min in tests.
- **Drift between agent and scheduler.** If an operator hand-edits a host's `compose.yaml`, the scheduler doesn't know. Document this in `PLACEMENT.md`; future work could ship a "scheduler is authoritative" mode that forbids local edits.
- **The `on:` host typo case.** A common mistake. The `placement.unknown_host` event is the operator's only feedback. Make sure `placement explain` clearly shows the typo case.

## Subagent dispatch

Per `feedback_implementer_opus`, implementers run on Opus. Reviewers (post-step) on Sonnet for cost.

| Step | Implementer | Reviewer |
|---|---|---|
| 1 | Opus | Sonnet (migration safety) |
| 2 | Opus | Sonnet (parser error UX) |
| 3 | Opus | Sonnet (boot integration) |
| 4 | Opus | none |
| 5 | Opus | none |
| 6 | Opus | Sonnet (reconcile correctness) |
| 7 | Opus | none |
| 8 | Opus | Sonnet (multi-host scenarios) |
| 9 | Opus | none |
| 10 | Opus | none |
| 11 | main | none |
| 12 | main | operator review |

## Open questions during implementation

- **Should `Scheduler::reconcile_all` fan out via `JoinSet` for parallelism?** Profile first; sequential is probably fine for fleets under 50 services. Decide in step 6.
- **gRPC method placement.** Plugin RPC vs a new dedicated service. Defer to step 9; both work, plugin RPC matches existing `isd` plumbing.
- **`placement explain` exclusion reasoning ordering.** First match wins or all-clauses-listed? Spec example shows first-clause; if implementation finds it confusing, switch to all-clauses.
- **Heartbeat label hash storage.** In-memory only, lost on controller restart. First post-restart heartbeat triggers a reconcile regardless. Acceptable churn.

## Default-and-document calls baked into this plan

The spec flags five places where operator review may change a default. The plan ships the spec defaults and includes a marker (see step 8 scenario 5) where a flipped default would change a test. Each call:

1. `spread: 1` normalized to `Singleton` (parse step).
2. `placements` rows backfilled at migration (step 1).
3. `where:` zero match stays Pending (step 6 + step 8 scenario 5).
4. Disconnect rejoin prefers fresh-host replica (step 6).
5. Per-service grace period deferred to 0.15+; fleet-wide only (step 3).

If operator review flips any of these, the spec gets a note + the corresponding test gets rewritten. None of them affect the data model or migration shape.
