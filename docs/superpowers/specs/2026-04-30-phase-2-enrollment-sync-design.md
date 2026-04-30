# Phase 2: Agent Enrollment + Sync Stream

**Date:** 2026-04-30
**Status:** proposed
**Branch:** `next`
**Scope:** Agent ↔ controller wire — proto schema, enrollment RPC, bidi sync stream, token auth, persistent inventory of hosts. End state: an agent connects to a controller, gets an `agent_id`, and continuously reports heartbeats; the controller persists every host and updates `last_seen` on each heartbeat.
**Out of scope:** container snapshots, container update flow, journal/events, dashboard, mTLS, multi-controller HA, Sia wire.

---

## 1. Why this exists

Phase 0+1 stood up the plugin host. The host can load plugins but has no way to talk across hosts. Phase 2 gives the controller and agent a real wire, persists what the controller learns, and replaces both runners' Phase 1 "immediate-return" stub with a real loop.

After Phase 2, the controller has a populated `hosts` table and the agent has a long-lived gRPC connection. **Phase 3 (the `updater` plugin port) builds on this** — once the agent has somewhere to send container state and updater results, the actual auto-update logic can land.

Phase 2 is itself a meaningful soak-test surface: friend could deploy a controller + agent today, see hosts come and go in `sqlite3 isengard.db`, with no real container management yet. That's not useful for friend (legacy Go binary remains the deploy target) but it validates the wire under real network conditions.

## 2. Decision summary

| # | Decision | Rationale |
|---|---|---|
| 1 | Two RPCs in `Controller` service: `Enroll` (unary) + `Sync` (bidi stream) | Enroll is one-shot (cheap, side-effecty). Sync is long-lived (carries every later push/command). One RPC each side, clean separation. |
| 2 | Token in **gRPC metadata** (`authorization: Bearer <ISENGARD_TOKEN>`), not message body | Standard HTTP/2 / gRPC convention. Implemented as a `tower` middleware so swapping to mTLS later is local. |
| 3 | Agent identity = controller-assigned **ULID**, persisted to `$ISENGARD_STATE/agent.json` on the agent host | Agent's ID survives restarts and reconnects. Controller can be wiped (re-enroll happens automatically). Crash-safe. |
| 4 | Agent reconnects with **exponential backoff** (1s → 60s, jittered), re-uses stored `agent_id` after first enroll | Survives controller restarts and network blips without extra protocol surface. |
| 5 | Heartbeats: agent sends one every **10s** by default; controller updates `last_seen_at` synchronously | Low overhead. Configurable later if needed. |
| 6 | Phase 2 message set: `Heartbeat` + `EnrollRequest` + `EnrollResponse` only. `ContainerSnapshot` / `Event` / `Command` are **scaffolded as oneof variants** but unused. | Keeps the proto stable so Phase 3-5 add variants without breaking wire compatibility. |
| 7 | Inventory storage: SQLite via `sqlx`, single table `hosts` in Phase 2 (containers + journal land in Phase 3 + 4). DB file at `$ISENGARD_STATE/isengard.db`. | Spec §7 already specified this; Phase 2 just builds the schema's first table. |
| 8 | TLS: **none in Phase 2.** Plain HTTP/2 + token. Operators wanting TLS terminate at a reverse proxy (Caddy/Traefik/nginx). mTLS later. | Spec §6.2 explicitly defers mTLS. Reverse-proxy pattern is the standard escape hatch. |
| 9 | New workspace crate: **`isengard-storage`** (SQLite layer). Factored out of `isengard-controller` so Phase 5 dashboard can use it without depending on the whole controller. | Clean boundary: storage is its own concern, used by both controller and (future) dashboard reads. |
| 10 | New workspace crate: **`isengard-test-harness`** (integration testing utilities, dev-only). | Spawns controller + N agents in-process, exposes assertions like "wait for agent X to be in inventory". Phase 2 introduces it; Phases 3-5 reuse. |
| 11 | Both binary subcommands gain a real **`tokio::signal::ctrl_c().await` shutdown loop**, replacing Phase 1's immediate-return stub. | Required for the agent to maintain a long-lived connection. |
| 12 | Logging: existing `tracing` setup; add structured `agent_id` and `host` fields via `tracing::Span` so events on the controller correlate per-agent. | Operational must-have for fleet debugging. |

## 3. Architecture

```
┌──────────────────────────────────────────────────┐
│ Controller (one process)                         │
│ ┌─────────────────────────────────────────────┐  │
│ │ tonic gRPC server (port 9417)               │  │
│ │ ┌─────────────────────────────────────────┐ │  │
│ │ │ TokenAuth tower middleware              │ │  │
│ │ │   reads `authorization: Bearer …` md    │ │  │
│ │ │   rejects with UNAUTHENTICATED on miss  │ │  │
│ │ └─────────────────────────────────────────┘ │  │
│ │   ↓                                         │  │
│ │ ┌──────────────┐    ┌─────────────────────┐ │  │
│ │ │ Enroll RPC   │    │ Sync bidi stream    │ │  │
│ │ │ (unary)      │    │ in:  AgentMessage   │ │  │
│ │ │              │    │ out: ControllerMsg  │ │  │
│ │ └──────┬───────┘    └──────────┬──────────┘ │  │
│ │        │                       │            │  │
│ │        ▼                       ▼            │  │
│ │   ┌────────────────────────────────────┐    │  │
│ │   │ isengard-storage::Inventory        │    │  │
│ │   │   sqlx + sqlite                    │    │  │
│ │   │   tables: hosts                    │    │  │
│ │   └────────────────────────────────────┘    │  │
│ └─────────────────────────────────────────────┘  │
│   ↑ ctrl_c shutdown signal terminates server     │
└──────────────────────────────────────────────────┘
                       ▲
                       │ HTTP/2 + gRPC
                       │ authorization: Bearer ISENGARD_TOKEN
                       │
┌──────────────────────┴───────────────────────────┐
│ Agent (one process per host)                     │
│ ┌─────────────────────────────────────────────┐  │
│ │ tonic gRPC client                           │  │
│ │ Enroll once → store agent_id                │  │
│ │ Sync stream loop (reconnect on drop)        │  │
│ │ Heartbeat every 10s in stream               │  │
│ └─────────────────────────────────────────────┘  │
│   ┌─────────────────────────────────────────┐    │
│   │ $ISENGARD_STATE/agent.json              │    │
│   │   { "agent_id": "01J..." }              │    │
│   └─────────────────────────────────────────┘    │
│   ↑ ctrl_c terminates the loop, drops the stream │
└──────────────────────────────────────────────────┘
```

Both sides are inside the same `isengard` binary (controller and agent subcommands of the existing CLI from Task 16). Phase 2 fills in the `run_controller` and `run_agent` library crate functions that today just call plugin lifecycle and return.

## 4. Wire contract (`isengard-proto/proto/isengard.v1.proto`)

```proto
syntax = "proto3";
package isengard.v1;

// Controller-side service. Agents call into it.
service Controller {
  // One-shot at agent startup. Agent presents host metadata, receives a
  // controller-assigned ULID + the heartbeat interval the controller wants.
  rpc Enroll(EnrollRequest) returns (EnrollResponse);

  // Long-lived bidirectional stream. Once enrolled, the agent opens this
  // stream and keeps it open. Reconnect on drop.
  //
  //   client → server: heartbeat + (Phase 3) container snapshots + (Phase 4) events
  //   server → client: heartbeat ack + (Phase 3) commands + (Phase 4) config updates
  rpc Sync(stream AgentMessage) returns (stream ControllerMessage);
}

// ============ ENROLL ============

message EnrollRequest {
  // Stable host fingerprint the agent picks (hostname is fine for v1; in the
  // future a hashed machine-id avoids collisions).
  string fingerprint = 1;
  string hostname    = 2;
  string os          = 3; // e.g. "linux/x86_64"
  string arch        = 4; // e.g. "x86_64"
  string agent_version  = 5; // semver of the agent binary
  string docker_version = 6; // queried from the local docker daemon
}

message EnrollResponse {
  // Controller-assigned ULID. Agent persists this and presents it as the
  // first message of the Sync stream on every (re)connect.
  string agent_id = 1;

  // How often the controller wants this agent to heartbeat. Default 10s.
  uint32 heartbeat_interval_secs = 2;

  // Server's view of "now". Used by agents that do not have NTP.
  uint64 server_time_ms = 3;
}

// ============ SYNC ============

message AgentMessage {
  oneof payload {
    // First message of every Sync stream. Identifies the agent.
    SyncHello   hello     = 1;
    Heartbeat   heartbeat = 2;
    // Phase 3+: ContainerSnapshot snapshot = 3;
    // Phase 4+: Event              event    = 4;
  }
}

message SyncHello {
  string agent_id = 1; // assigned during Enroll
}

message Heartbeat {
  uint64 ts_ms = 1; // agent's local clock
}

message ControllerMessage {
  oneof payload {
    HeartbeatAck heartbeat_ack = 1;
    // Phase 3+: Command       command       = 2;
    // Phase 4+: ConfigUpdate  config_update = 3;
  }
}

message HeartbeatAck {
  uint64 server_time_ms = 1;
}
```

`isengard-proto/build.rs` invokes `tonic-build` to generate the Rust client/server code at build time. The `oneof` slots scaffolded for Phase 3+ keep the wire stable.

## 5. Auth handshake

Token is set on both sides via env var `ISENGARD_TOKEN`. The token is a single shared secret in v1 — no per-agent tokens, no rotation flow.

Implementation:

- **Controller (server side)**: a `tower::ServiceBuilder::layer(TokenAuthLayer::new(token))` wraps the gRPC service. The layer reads `authorization` from request metadata, strips the `Bearer ` prefix, constant-time-compares against the configured token, and rejects on miss with `Status::unauthenticated`. Token is loaded from `ISENGARD_TOKEN` at startup; missing token is a startup error.
- **Agent (client side)**: a `tonic::Interceptor` injects `authorization: Bearer <token>` on every outgoing request including the `Sync` stream. Token loaded from `ISENGARD_TOKEN`; missing token is a startup error.

mTLS upgrade path: replace `TokenAuthLayer` with mTLS verification at the same point in the tower stack. Application code unchanged.

## 6. Agent lifecycle (Phase 2)

```
                   ┌─────────────────────────────────┐
                   │ agent boot                      │
                   │ load $ISENGARD_TOKEN            │
                   │ load controller URL             │
                   │ open $ISENGARD_STATE/agent.json │
                   └──────────────┬──────────────────┘
                                  │
                  ┌───────────────┴───────────────┐
            no agent_id yet                  agent_id present
                  │                               │
                  ▼                               ▼
        ┌─────────────────────┐         ┌─────────────────────┐
        │ Enroll RPC          │         │ Skip enroll          │
        │ store agent_id     │         │                     │
        └──────────┬──────────┘         └──────────┬──────────┘
                   │                               │
                   └───────────────┬───────────────┘
                                   │
                                   ▼
                    ┌─────────────────────────────┐
                    │ open Sync stream            │
                    │ first send: SyncHello       │
                    │ then: Heartbeat every 10s   │
                    └──────────┬──────────────────┘
                               │
                  on stream drop / network error
                               │
                               ▼
                    ┌─────────────────────────────┐
                    │ exponential backoff          │
                    │ 1s, 2s, 4s, 8s, 16s, 32s,    │
                    │ 60s (cap), jittered ±20%    │
                    └──────────┬──────────────────┘
                               │
                               └──→ retry "open Sync stream"
```

Backoff resets to 1s after a successful stream stays open for ≥ 60s (avoid thrashing if connections are short-lived; reward stability).

`tokio::signal::ctrl_c` cancels the loop, gracefully drops the stream, calls plugin `stop()` hooks, exits 0.

## 7. Controller lifecycle (Phase 2)

```
boot:
  load $ISENGARD_TOKEN (required)
  open SQLite DB at $ISENGARD_STATE/isengard.db
  run sqlx migrations (creates `hosts` table on first boot)
  bind tonic gRPC server on listen addr (default 0.0.0.0:9417)
  call plugin lifecycle: init → start
  await ctrl_c

per Enroll RPC:
  validate token (middleware)
  generate ULID for agent_id
  insert into hosts (id, fingerprint, hostname, os, arch, agent_version, docker_version, enrolled_at, last_seen_at)
  return EnrollResponse{agent_id, heartbeat_interval_secs=10, server_time_ms}

per Sync stream:
  validate token (middleware)
  read first message: must be SyncHello
  look up host by agent_id; if missing → return UNAUTHENTICATED (forces re-enroll)
  loop:
    read next AgentMessage
    if Heartbeat:
      UPDATE hosts SET last_seen_at=now() WHERE id=agent_id
      send back ControllerMessage{HeartbeatAck{server_time_ms}}
    else:
      log + ignore (Phase 3+ variants)
  on stream end / error: log "agent disconnected", continue serving other agents

shutdown:
  graceful drain (let in-flight requests finish), then exit
  call plugin lifecycle: stop
```

## 8. Storage

SQLite via `sqlx`, file at `$ISENGARD_STATE/isengard.db` (default `$ISENGARD_STATE` = `/var/lib/isengard` in container, `~/.local/state/isengard` for local dev).

### Phase 2 migration: `0001_hosts.sql`

```sql
CREATE TABLE hosts (
    id              BLOB     PRIMARY KEY,             -- ULID, 16 bytes
    fingerprint     TEXT     NOT NULL UNIQUE,
    hostname        TEXT     NOT NULL,
    os              TEXT     NOT NULL,
    arch            TEXT     NOT NULL,
    agent_version   TEXT     NOT NULL,
    docker_version  TEXT     NOT NULL,
    enrolled_at     INTEGER  NOT NULL,                 -- unix seconds
    last_seen_at    INTEGER,                           -- unix seconds
    metadata        TEXT     NOT NULL DEFAULT '{}'    -- json blob for future use
);

CREATE INDEX hosts_last_seen_at_idx ON hosts(last_seen_at DESC);
```

`containers` and `journal` tables ship in Phase 3 + 4 migrations.

### `isengard-storage` crate API (Phase 2 surface)

```rust
pub struct Inventory { /* sqlx::SqlitePool */ }

impl Inventory {
    pub async fn open(path: &Path) -> Result<Self>;     // runs migrations
    pub async fn enroll_host(&self, req: EnrollHost) -> Result<HostId>;
    pub async fn touch_host(&self, id: HostId, ts: u64) -> Result<()>;
    pub async fn get_host(&self, id: HostId) -> Result<Option<Host>>;
    pub async fn list_hosts(&self) -> Result<Vec<Host>>;
}
```

`HostId` is a wrapper around `[u8; 16]` (ULID). Public surface is async + serde-clean for future dashboard use.

## 9. Crate changes

### Modified

| Crate | Change |
|---|---|
| `isengard-proto` | Add `proto/isengard.v1.proto`, `build.rs` invoking `tonic-build`. Currently a placeholder; this task gives it real content. |
| `isengard-controller` | Replace `run_controller` body: bind gRPC server, install tower middleware, register `Controller` service impl, await ctrl_c. |
| `isengard-agent` | Replace `run_agent` body: load/persist agent_id, run enroll-or-skip, run Sync loop with backoff, await ctrl_c. |
| `isengard` (binary) | Pass `--listen` (controller) and `--controller` (agent) flags through to runners; require `ISENGARD_TOKEN` env at startup; expose `--state-dir` (default `~/.local/state/isengard` for dev, `/var/lib/isengard` in container). |

### Added

| Crate | Purpose |
|---|---|
| `isengard-storage` | SQLite + sqlx. Public `Inventory` type. Migrations in `migrations/`. Used by `isengard-controller`. |
| `isengard-test-harness` | dev-only. Spawns controller + N agents in-process on ephemeral ports. Used by `isengard-tests`. |

### Workspace dep additions

| Crate | Why |
|---|---|
| `tonic` 0.12 | gRPC client + server |
| `tonic-build` 0.12 | proto codegen (build dep of `isengard-proto`) |
| `prost` 0.13 | protobuf runtime, used via tonic |
| `tower` 0.5 | middleware stack for auth |
| `sqlx` 0.8 (`runtime-tokio`, `sqlite`, `migrate`, `macros`) | SQLite driver + migrations |
| `ulid` 1.x | ULID generation for `HostId` |
| `tokio-stream` 0.1 | bidi stream helpers |
| `rand` 0.8 | jittered backoff |

`bollard` is **not** added in Phase 2 — it lands in Phase 3 with the updater plugin.

## 10. Testing strategy

- **Unit tests** per crate (storage CRUD, backoff jitter math, token middleware accept/reject).
- **Integration test** in `isengard-tests/tests/phase2_enroll_sync.rs`:
  1. `harness::spawn_controller(token, ephemeral_port).await`
  2. `harness::spawn_agent(token, controller_url).await`
  3. Wait up to 5s for `inventory.list_hosts()` to contain 1 row → assert `enrolled_at` is set
  4. Wait up to 15s for that row's `last_seen_at` to advance ≥ 1 heartbeat → assert it bumped
  5. Drop the agent (cancel its task)
  6. Restart agent with the same `agent.json` state dir → assert it does NOT re-enroll (still 1 row), but `last_seen_at` continues bumping
  7. Restart controller (drop + reopen the SQLite file, re-bind port) → assert agent reconnects within 30s and resumes heartbeats
- **Property test** on backoff: assert `next_backoff` is monotonic up to the cap and never exceeds 60s × 1.2.
- **Negative tests**: enroll with bad token → UNAUTHENTICATED; sync without `SyncHello` first → invalid argument; sync with unknown `agent_id` → UNAUTHENTICATED.

CI runs unit + integration. Property tests run on `main`/`next` pushes only.

## 11. Done state

Phase 2 is done when:

1. `cargo build --workspace` clean. `cargo nextest run --workspace` passes (unit + integration tests).
2. `cargo deny check` green.
3. Running `ISENGARD_TOKEN=test isengard controller` and `ISENGARD_TOKEN=test isengard agent --controller=http://localhost:9417` in two terminals: agent enrolls, heartbeats every 10s, both visible in tracing logs.
4. After agent crash: agent restarts cleanly without re-enrolling, controller's `last_seen_at` continues advancing.
5. After controller crash: agent reconnects with backoff, resumes heartbeats; controller sees the same agent_id (no duplicate row).
6. `sqlite3 ~/.local/state/isengard/isengard.db "select * from hosts;"` shows the agent row.
7. ≥ 25 tests in the workspace (16 from Phase 0+1 + ≥ 9 new for Phase 2).
8. Tag `v0.2.0-alpha.wire` set locally on the merge commit.

## 12. Out of scope

Defer to later phases:

- Container snapshot streaming (Phase 3)
- Container update commands controller→agent (Phase 3)
- Event journal + notifier subscriber (Phase 4)
- Dashboard reads (Phase 5)
- mTLS authentication (post-v1)
- Per-agent tokens / token rotation (post-v1)
- TLS termination configuration (operator's reverse proxy responsibility for v1)
- Multi-controller HA (post-v1)
- Agent group / labels / metadata-driven targeting (post-v1)
- Compression / batching for streams (premature; revisit when fleet > 50 hosts)

## 13. Implementation phases (within Phase 2)

The Phase 2 plan splits into sub-phases for incremental landability. Each ends in a green workspace and a meaningful tag-able state.

| Sub-phase | Scope | Done when |
|---|---|---|
| 2a | proto + tonic-build + empty server skeleton | `isengard-proto` generates code; `isengard controller` binds and accepts (rejects all RPCs since no impl). |
| 2b | `isengard-storage` crate + `Inventory` API + migration | Unit tests for CRUD pass. |
| 2c | `Enroll` RPC + token middleware (server side) | Manual `grpcurl` against running controller succeeds with token, fails without. |
| 2d | Agent client: enroll + persist agent_id | Agent runs, contacts controller, sees agent_id appear on disk and in inventory. |
| 2e | `Sync` RPC + heartbeat loop (both sides) | Heartbeats visible, `last_seen_at` bumps. |
| 2f | Reconnection + state-dir resume | Crash tests in §10 pass. |
| 2g | Integration test harness + full Phase 2 test suite | All 9+ integration tests green in CI. |

Each sub-phase gets one plan written when its predecessor lands.

## 14. Open questions

- **Heartbeat interval configurability**: Phase 2 hardcodes 10s. Make it controller-configurable per-agent (set via `EnrollResponse.heartbeat_interval_secs`)? Spec already has the field; just need to make the agent honor it. Simple, can ship in 2e.
- **Should `EnrollRequest.fingerprint` be machine-id-derived rather than hostname-derived?** Hostname can collide (containers, hosting providers). `/etc/machine-id` SHA-256 is more stable. Defer decision until friend's deploy reveals which is needed.
- **Graceful agent shutdown**: should the agent send a final "goodbye" message before closing the stream so the controller can mark it cleanly disconnected (rather than waiting for heartbeat timeout)? Probably yes; cheap. Add to 2f.
- **What's the heartbeat-timeout threshold for "agent unreachable"?** Spec §11 mentioned 5min / 4h. Phase 2 ships without surfacing this — `last_seen_at` is just a column. The dashboard (Phase 5) will compute "unreachable" client-side based on a configurable threshold.

## 15. References

- Spec v1: `2026-04-29-platform-pivot-design.md` — sections §3, §6, §7
- Phase 0+1 plan: `2026-04-29-platform-foundation.md`
- Project Context: `weavers-vault/1 Projects/Isengard/Context.md`
