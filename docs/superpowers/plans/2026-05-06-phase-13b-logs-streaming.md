# Phase 13 Plan B: Logs streaming

Implementation plan for [[2026-05-06-phase-13b-logs-streaming-design]]. Closes issue #57.

Branch: `feat/phase-13b`
Worktree: `~/Projects/isengard/.worktrees/phase-13b`
Base: `next` at `25848ee`
Migration slot: not needed (logs are not persisted; the fanout is in-memory).

## Standing self-review (every task)

Before declaring done:

1. `cargo build --workspace`
2. `cargo test -p <crate>` for the touched crate; full workspace tests on the wrap-up sweep
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Grep changed files for em dash (U+2014) and en dash (U+2013): zero tolerance
6. `bun run build` in `crates/isengard-plugins/dashboard/web` for UI tasks
7. Cite exact files added/modified

## Tasks

### T1: Proto + subscription types

**Goal**: extend the wire schema with the log-streaming subscription.

**Files**:
- `crates/isengard-proto/proto/isengard.v1.proto`: add `LogChunk`, `StartLogStream`, `StopLogStream`, plug them into the existing `AgentMessage` / `ControllerMessage` oneofs.
- `crates/isengard-proto/tests/proxy_config_roundtrip.rs` (or new `log_chunk_roundtrip.rs`): proto round-trip test.

**Acceptance**: `cargo test -p isengard-proto` green. New variants are stable wire numbers (no reuse of existing numbers).

### T2: Agent-side bollard log tailer

**Goal**: ship the log producer that runs on each agent.

**Files**:
- `crates/isengard-agent/src/logs.rs` (new): `tail_container_logs(docker, container, tail, follow, abort_rx, out_tx)`.
  Reads `bollard::container::LogsOptions { follow:true, stdout:true, stderr:true, timestamps:true, tail }`. Decodes each `LogOutput`, splits on `\n`, parses the docker timestamp prefix, sends `LogChunk` frames into `out_tx`. Honors `abort_rx` (a `tokio::sync::watch::Receiver<bool>`) for cancellation.
- `crates/isengard-agent/src/lib.rs`: declare the module; on Sync inbound, route `StartLogStream` / `StopLogStream` to a small subscription map. `StartLogStream` spawns a tail task; `StopLogStream` flips its abort.
- `crates/isengard-agent/src/sync.rs`: handle the two new ControllerMessage variants in the inbound loop. Pass the `agent_msg_tx` so chunks can be funneled back as `AgentMessage::LogChunk`.
- `crates/isengard-agent/tests/logs_unit.rs` (new): 4 tests. Mocked via a thin trait `LogSource` so we do not need a live Docker daemon (line splitting, abort, multi-line, error -> unavailable).

**Acceptance**: 4 tests pass. The module compiles without bollard daemon access.

### T3: Controller-side fanout

**Goal**: turn agent-emitted `LogChunk` frames into a per-subscription mpsc the dashboard reads.

**Files**:
- `crates/isengard-controller/src/log_fanout.rs` (new): `LogFanout` registry mapping subscription_id -> `mpsc::Sender<LogChunk>`. `register(sub_id, host_ids) -> Receiver<LogChunk>`, `unregister(sub_id)`. Idempotent.
- `crates/isengard-controller/src/service.rs`: in the Sync inbound loop, route `LogChunk` frames into the registry. On no-match, drop the frame and warn.
- `crates/isengard-controller/src/lib.rs`: surface `Arc<LogFanout>` on `ControllerHandles` so the dashboard plugin can call into it.

**Acceptance**: registry unit-tests green. No regression on existing controller tests.

### T4: Dashboard WebSocket endpoint

**Goal**: ship `GET /api/v1/services/:stack_id/:service_name/logs/ws` on the dashboard plugin.

**Files**:
- `crates/isengard-plugins/dashboard/src/ws.rs`: replace the existing placeholder `handle_logs` with `handle_service_logs(ws, stack_id, service_name)`. Resolves stack -> service -> hosts (primary + other_instances). Generates a subscription_id. Calls `LogFanout::register` to get a Receiver. For each host, sends `ControllerMessage::StartLogStream` via `RoutingPusher::register_sender` (reused as the per-host outbound sender). Drains the Receiver into the WebSocket, JSON-encoding frames. On client disconnect: send `StopLogStream` to each host, unregister.
- `crates/isengard-plugins/dashboard/src/lib.rs`: change the WS route to `/api/v1/services/{stack_id}/{service_name}/logs/ws`. Drop the legacy `/api/v1/services/{id}/logs` route.
- `crates/isengard-plugins/dashboard/tests/logs_ws.rs` (new): 6 tests. Use an in-process `axum::Router` + `tower::ServiceExt::oneshot` on the WS upgrade path; for the multi-host test, push synthetic `LogChunk`s through the fanout directly (we do not need a real Sync stream for the unit-level coverage).

**Acceptance**: 6 tests pass. The placeholder code path on the dashboard is removed.

### T5: UI `<LogsPanel>` + composable refresh

**Goal**: replace the placeholder card with the real component.

**Files**:
- `crates/isengard-plugins/dashboard/web/composables/useLogStream.ts`: rewrite around the new wire protocol. Returns `{ lines, state, dropped, hosts, pause, resume, setLevel, setTail, connect, disconnect }`. Tracks per-host counters. Hard cap 5000 lines.
- `crates/isengard-plugins/dashboard/web/components/LogsPanel.vue` (new): renders the panel. Tabs (one per host + an Aggregate tab). Toolbar (tail-N slider, pause/resume button, level dropdown, regex/substring filter input). Scroll container with auto-scroll-to-bottom + "Jump to live" pill. Error / empty states.
- `crates/isengard-plugins/dashboard/web/pages/stacks/[id]/services/[name].vue`: drop the dashed-border placeholder section; mount `<LogsPanel :stack-id="stackId" :service-name="serviceName" :hosts="hostList" />` in its place.

**Acceptance**: `bun run build` green. Component renders. No console errors when wired against a stub WebSocket.

### T6: Design tracker + release notes

**Goal**: document what shipped.

**Files**:
- `design/pages/service-detail.md`: status flips from `partial` to `shipped` (or, more precisely, "13B shipped, 13C-13J still queued"). Drop the "logs panel pending 13B" note.
- `docs/RELEASE_NOTES_PHASE_13B.md` (new): operator-facing summary.

**Acceptance**: both files committed.

### T7: Final gate sweep + PR

**Goal**: workspace gates green, branch pushed, PR opened against `next`.

Steps:
1. `cargo build --workspace`
2. `cargo test --workspace`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. `cd crates/isengard-plugins/dashboard/web && bun install && bun run build`
6. `git push -u origin feat/phase-13b`
7. `gh pr create --base next --title "feat: phase 13b (logs streaming)" --body "Closes #57 ..."`

**Acceptance**: PR URL captured. CI green or expected to be green.

## File map

```
crates/isengard-proto/proto/isengard.v1.proto                                   (modify)
crates/isengard-proto/tests/log_chunk_roundtrip.rs                              (new)
crates/isengard-agent/src/logs.rs                                               (new)
crates/isengard-agent/src/lib.rs                                                (modify)
crates/isengard-agent/src/sync.rs                                               (modify)
crates/isengard-agent/tests/logs_unit.rs                                        (new)
crates/isengard-controller/src/log_fanout.rs                                    (new)
crates/isengard-controller/src/service.rs                                       (modify)
crates/isengard-controller/src/lib.rs                                           (modify)
crates/isengard-plugins/dashboard/src/ws.rs                                     (modify)
crates/isengard-plugins/dashboard/src/lib.rs                                    (modify)
crates/isengard-plugins/dashboard/tests/logs_ws.rs                              (new)
crates/isengard-plugins/dashboard/web/composables/useLogStream.ts               (modify)
crates/isengard-plugins/dashboard/web/components/LogsPanel.vue                  (new)
crates/isengard-plugins/dashboard/web/pages/stacks/[id]/services/[name].vue     (modify)
design/pages/service-detail.md                                                  (modify)
docs/superpowers/specs/2026-05-06-phase-13b-logs-streaming-design.md            (new)
docs/superpowers/plans/2026-05-06-phase-13b-logs-streaming.md                   (new)
docs/RELEASE_NOTES_PHASE_13B.md                                                 (new)
```

## Risks

- The piggyback on the Sync stream means a single agent's log flood can compete with heartbeats for stream bandwidth. The bounded queue + drop-on-overflow on both ends keeps this contained; debug-mode apps spewing 10K lines/sec degrade to ~500 lines/sec at the controller, with explicit `dropped` notifications.
- Subscription leaks: if a controller crashes mid-stream, the agent keeps tailing until the Sync stream drops. The agent's tail tasks abort when the Sync stream errors (the `agent_msg_tx` receiver closes). Belt-and-braces: we do not persist subscription state, so a controller restart starts from zero.
- Multi-host fanout assumes the controller owns every host involved. The dashboard plugin already does so by virtue of being on the controller; multi-controller deployments are out of scope.
