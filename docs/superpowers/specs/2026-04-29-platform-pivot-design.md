# Isengard v1: container management platform

**Date:** 2026-04-29
**Status:** proposed
**Branch:** `feat/platform-rewrite`
**Scope:** v1 architecture, language, plugin model, wire protocol, dashboard surface, repo structure
**Out of scope (deferred):** v1.x plugins (scheduler, metrics, rollback, hooks), mTLS, WASM plugins, multi-controller HA, Sia wire format, micro-VM substrate

---

## 1. Why this exists

Isengard today is a single-host Docker auto-updater (Watchtower replacement, ~2083 LOC of Go, 3 GitHub stars, public). A friend wants to deploy it on a small fleet (2–5 hosts, no orchestrator, wants a single pane of glass). That pull surfaces the question: is Isengard a tool, or is it a platform?

This spec commits to the platform answer. Today's auto-updater becomes one plugin. The host process gains an agent/controller architecture, a plugin model, a journaled event stream, and a dashboard. The friend's deploy is the v1 design partner; the v1 deliverable is "useful product for the friend, with a taste of what the platform becomes."

The long-term direction is owning the stack down to micro-VMs and possibly a custom container runtime. v1 must not foreclose that, but doesn't try to do it.

## 2. Decision summary

| # | Decision | Rationale (short) |
|---|---|---|
| 1 | Pivot from tool to platform | First external pull validates platform framing |
| 2 | Multi-host: full controller + agents (not single-host, not status-only viewer) | Friend wants a single pane and accepts multi-week build |
| 3 | Language: full Rust, no hybrid | Memory safety, WASM-readiness, ecosystem (bollard, tonic, axum, leptos), Loom continuity |
| 4 | Single binary with subcommands (`isengard agent`, `isengard controller`) | One artifact to ship, shared plugin host, fewer moving parts |
| 5 | Plugin model: compile-time Rust traits + `inventory` registration, contract serde-clean for future WASM | Caddy-style: ship today, sandbox tomorrow without breaking authors |
| 6 | Plugin naming: functional, no LOTR theming | Code reads like documentation; brand lives in the dashboard, not in plugin paths |
| 7 | v1 plugin set: `updater`, `dashboard`, `notifier` | Minimum trust kit; `scheduler` / `metrics` / `rollback` / `hooks` defer to v1.x |
| 8 | Wire protocol: gRPC via `tonic` | Bidi streams for status, evolves cleanly, native Rust ecosystem; Sia is a separate future project |
| 9 | Auth (agent ↔ controller): bearer token via env var | Ship-now choice; mTLS upgrade designed in but not v1 |
| 10 | Storage: embedded SQLite via `sqlx` | One file, no service to run, ad-hoc queries for dashboard |
| 11 | Dashboard surface: Celestial Atlas (Leptos + axum, mounted by `dashboard` plugin) | Avant-garde brand surface: hosts as stars, containers as planets, constellations as service deps, parchment-and-brass aesthetic |
| 12 | Repo structure: Cargo workspace, no Turborepo, no Bun for Rust side | Cargo workspaces are the Rust-native monorepo; Justfile for cross-stack tasks |
| 13 | Migration: rewrite via tests, not port | Re-derive subtle correctness through new test suite; Go `main` stays until Rust v1 ships |

## 3. Architecture overview

```
                    ┌────────────────────────────────────┐
                    │           Controller               │
                    │  (one process, any host)           │
                    │                                    │
  registries ◄───── │   Core:                            │
  (digest checks    │   • plugin host                    │
   happen on agents) │   • gRPC server (tonic)           │
                    │   • inventory store (sqlx/sqlite)  │
                    │   • event journal (append-only)    │
                    │   • config distribution            │
                    │                                    │
                    │   Plugins:                         │
                    │   • dashboard  (Leptos + axum)     │
                    │   • notifier   (event subscriber)  │
                    └─────────────────┬──────────────────┘
                                      │ gRPC over TCP
                                      │ bearer token (ISENGARD_TOKEN)
                                      │ bidi streams for status
              ┌───────────────────────┼───────────────────────┐
              ▼                       ▼                       ▼
       ┌──────────────┐        ┌──────────────┐        ┌──────────────┐
       │ Agent · web-1│        │ Agent · data-1│        │ Agent · edge-1│
       │              │        │              │        │              │
       │ Core:        │        │ Core:        │        │ Core:        │
       │ • plugin host│        │ • plugin host│        │ • plugin host│
       │ • gRPC client│        │ • gRPC client│        │ • gRPC client│
       │              │        │              │        │              │
       │ Plugins:     │        │ Plugins:     │        │ Plugins:     │
       │ • updater    │        │ • updater    │        │ • updater    │
       └──────┬───────┘        └──────┬───────┘        └──────┬───────┘
              ▼                       ▼                       ▼
       [docker socket]         [docker socket]         [docker socket]
```

Same binary, two subcommands. Controller and agent share the plugin host code path. v1 only registers the three v1 plugins; future plugins land as new crates.

## 4. Repo structure

Cargo workspace at the repo root. The existing `www/` Nuxt landing site at isengard.app stays untouched.

```
isengard/
├── Cargo.toml              # [workspace]
├── Cargo.lock
├── Justfile                # cross-stack tasks
├── README.md
├── LICENSE
├── crates/
│   ├── isengard-core/      # plugin host, traits, lifecycle, journal types
│   ├── isengard-proto/     # .proto files + tonic-generated code
│   ├── isengard/           # binary crate — entry point, parses subcommand, dispatches
│   ├── isengard-controller/  # library crate — controller-mode logic, plugin registry
│   ├── isengard-agent/     # library crate — agent-mode logic, plugin registry
│   └── isengard-plugins/
│       ├── updater/        # agent-side: existing Watchtower logic, ported
│       ├── dashboard/      # controller-side: Leptos SSR + axum routes
│       └── notifier/       # controller-side: event subscriber → Slack/Discord/webhook
├── www/                    # existing Nuxt landing site, untouched
├── docs/
│   └── superpowers/specs/  # this file lives here
└── docker/
    ├── Dockerfile          # multi-stage, scratch-based, single binary
    └── compose.yaml        # local dev stack (controller + 2 agents + nginx test)
```

**Tooling:**

- **`cargo` workspace** — the monorepo
- **`cargo-nextest`** — faster test runner with better output
- **`just`** — Justfile is the single entry point for every command (`just build`, `just dev`, `just lint`, `just www`)
- **`sccache`** — compile cache for CI and rebuilds
- **`cargo-deny`** — license / security / duplicate-dependency checks in CI

## 5. Plugin model

### 5.1 Trait shape

One core trait `Plugin` with lifecycle hooks. Plugins opt into capability sub-traits to declare what side of the system they run on and what they do.

```rust
// crates/isengard-core/src/plugin.rs

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Stable plugin identifier (e.g. "updater", "notifier"). Used for config namespacing
    /// and journal events. Must match the directory name under crates/isengard-plugins/.
    fn name(&self) -> &'static str;

    /// Semantic version of this plugin. Plugins evolve independently of the host.
    fn version(&self) -> &'static str;

    /// Called once at host startup. Plugin reads its slice of the config object,
    /// returns Err if config is invalid. Host aborts startup on Err.
    async fn init(&mut self, ctx: &PluginContext, config: serde_json::Value) -> Result<()>;

    /// Called after init succeeds. Plugin spawns its tasks here.
    async fn start(&mut self, ctx: &PluginContext) -> Result<()>;

    /// Called on graceful shutdown. Plugin must stop its tasks within ctx.shutdown_grace.
    async fn stop(&mut self) -> Result<()>;
}

/// Capability: this plugin runs on agents and exposes an update lifecycle.
#[async_trait]
pub trait AgentPlugin: Plugin {
    /// Run one cycle of whatever this plugin does on the agent.
    /// Updater: check digests, recreate containers. Future hooks plugin: fire pre/post.
    async fn run_cycle(&self, ctx: &AgentContext) -> Result<CycleReport>;
}

/// Capability: this plugin runs on the controller and reacts to journal events.
#[async_trait]
pub trait EventSubscriber: Plugin {
    /// Filter — which event types this subscriber wants. Empty = all.
    fn subscribed_events(&self) -> &[EventKind];

    /// Called on every matching event. Errors are logged, do not stop the journal.
    async fn handle(&self, event: &Event, ctx: &ControllerContext) -> Result<()>;
}

/// Capability: this plugin mounts HTTP routes onto the controller's axum router.
/// Used by `dashboard` (mounts /, /api/*) and any future plugin exposing UI/API.
pub trait HttpHandler: Plugin {
    fn mount(&self, router: axum::Router) -> axum::Router;
}
```

A plugin can implement multiple capabilities. The `dashboard` plugin implements `Plugin + HttpHandler`. The `notifier` plugin implements `Plugin + EventSubscriber`. The `updater` plugin implements `Plugin + AgentPlugin`.

### 5.2 Registration

Plugins register themselves at compile time using the `inventory` crate, no central enable/disable list to maintain:

```rust
// crates/isengard-plugins/updater/src/lib.rs
inventory::submit! {
    PluginRegistration {
        name: "updater",
        constructor: || Box::new(Updater::new()),
        capabilities: &[Capability::Agent],
    }
}
```

Host enumerates registrations at startup, instantiates plugins matching the current mode (controller or agent), passes each its config slice.

### 5.3 Contract discipline (WASM-ready)

All inputs and outputs that cross the plugin boundary must serialize via `serde`. No host pointers, no callbacks holding references, no shared mutable state. `PluginContext` exposes services (logger, config reader, journal writer, gRPC handles) through methods that take/return owned/serde types — never lend internal state.

This is the same discipline `wasmtime` requires. Plugin authors writing against the v1 trait will not need to change their code when WASM-loaded plugins are added later; only the host's loader changes.

## 6. Wire protocol

### 6.1 gRPC service surface

Defined in `crates/isengard-proto/proto/isengard.proto`. Two services:

```proto
syntax = "proto3";
package isengard.v1;

// Controller exposes this service. Agents call into it.
service Controller {
  // Initial enrollment. Agent presents token + host info, gets back its agent_id
  // and the current applicable config.
  rpc Enroll(EnrollRequest) returns (EnrollResponse);

  // Status reporting. Agent streams snapshots of its container state and any
  // events it observed. Server responds with config updates and commands.
  // Bidi stream — the only long-lived connection per agent.
  rpc Sync(stream AgentMessage) returns (stream ControllerMessage);
}

// Dashboard plugin exposes this service over the same gRPC server.
// Used by the Leptos frontend (and external clients) to query state and
// trigger commands. JSON access via grpc-gateway-style HTTP transcoding —
// or simpler: dashboard mounts its own JSON HTTP API on the same axum router.
service Inventory {
  rpc ListHosts(ListHostsRequest) returns (ListHostsResponse);
  rpc ListContainers(ListContainersRequest) returns (ListContainersResponse);
  rpc StreamEvents(StreamEventsRequest) returns (stream Event);
  rpc TriggerUpdate(TriggerUpdateRequest) returns (TriggerUpdateResponse);
}
```

### 6.2 Auth

Bearer token in gRPC metadata. Token is a single shared secret (`ISENGARD_TOKEN`) provisioned in env on each agent and the controller. v1.

mTLS-ready: `tonic` supports mTLS natively. The token check is implemented as a `tower` middleware so swapping in cert verification is a localized change. Designed in, not built.

### 6.3 Sia (deferred)

A future spec covers `rust-sia` and the migration of Isengard's wire format from gRPC to Sia. The agent↔controller boundary is hidden behind generated client/server code; swapping wire formats is a contained rewrite, not a project-wide rewrite. Not v1.

## 7. Storage

Embedded SQLite via `sqlx`, one file per controller process at `$ISENGARD_STATE/isengard.db` (default `$ISENGARD_STATE` = `/var/lib/isengard` in container, `~/.isengard` for local dev). Primary keys are ULIDs (lexicographically sortable, time-ordered, 128-bit) for compact index locality and human-debuggable timestamps.

### 7.1 Schema (sketch)

```sql
CREATE TABLE hosts (
  id            BLOB PRIMARY KEY,            -- ULID
  name          TEXT NOT NULL,
  enrolled_at   INTEGER NOT NULL,            -- unix seconds
  last_seen_at  INTEGER,
  agent_version TEXT,
  os            TEXT,
  docker_version TEXT,
  metadata      TEXT                          -- json
);

CREATE TABLE containers (
  id            BLOB PRIMARY KEY,             -- ULID
  host_id       BLOB NOT NULL REFERENCES hosts(id),
  docker_id     TEXT NOT NULL,                -- 64-char id
  name          TEXT NOT NULL,
  image         TEXT NOT NULL,
  image_id      TEXT NOT NULL,
  state         TEXT NOT NULL,                -- running|exited|...
  last_check_at INTEGER,
  last_update_at INTEGER,
  health        TEXT,                          -- ok|warn|fail|unknown
  labels        TEXT                           -- json
);

CREATE TABLE journal (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  ts            INTEGER NOT NULL,              -- unix millis
  kind          TEXT NOT NULL,                 -- update.success|update.failed|agent.connect|...
  host_id       BLOB,
  container_id  BLOB,
  plugin        TEXT,
  payload       TEXT NOT NULL                  -- json, schema per kind
);

CREATE INDEX journal_ts ON journal(ts DESC);
CREATE INDEX journal_kind_ts ON journal(kind, ts DESC);
```

Journal is append-only. Inventory is mutated as state changes. Both queryable from the dashboard plugin.

### 7.2 Backups

`isengard backup` subcommand writes a consistent SQLite snapshot to a path. Friend's prod runs this in a daily cron. Not v1 scope but the file shape (one .db file) makes it trivial to add.

## 8. Dashboard

### 8.1 Direction

Celestial Atlas. Mocked at fidelity in `.superpowers/brainstorm/93385-1777487414/content/atlas-home.html` (in vault, not repo).

- **Hosts as stars** — luminance encodes health (bright = healthy, warm-orange = warn, dim = unreachable, red pulse = failed)
- **Containers as planets** — orbiting their host star, labeled with image:tag
- **Constellations** — service-to-service dependencies (network links between containers across hosts)
- **Comets** — updates in flight, animated
- **Astrolabe** — top-right widget shows current time and next maintenance window
- **Inscription bar** — bottom row: live event ticker (last sighting, in-flight, unreachable, next window)
- **Cartouche** — top-left: fleet stat counts
- **Aesthetic** — parchment-and-brass on midnight, Georgia italic for proper names, monospace for technical detail

### 8.2 Tech

- **Leptos** for SSR + WASM hydration. Single rust-only frontend. Compiles to a `dashboard.wasm` blob served from the controller binary's embedded static assets.
- **`axum`** as the HTTP server. The dashboard plugin mounts `/` (atlas), `/host/:id`, `/container/:id`, `/api/*` on the controller's shared `axum::Router`.
- **No JS toolchain on the Rust side.** The `www/` Nuxt site stays separate (it's the public marketing site, different audience and lifecycle).

If Leptos turns out to bite — slow rebuilds, missing libraries, painful debug — the swap to a JS frontend is contained: replace the dashboard plugin's UI rendering with a static SPA bundle, JSON API stays the same. Bounded blast radius.

## 9. v1 plugin responsibilities

### 9.1 `updater` (agent-side)

Carries the existing Watchtower-replacement logic, ported to Rust:

- List running containers (filter by `isengard.enable` label, watch-all default)
- For each candidate, check remote registry digest via HEAD request (~50ms)
- Compare against local `RepoDigests`
- If different: pull new image, recreate container preserving config (ports, mounts, networks, env, labels, restart policy)
- Self-update: rename-then-replace ordering so the replacement starts before the original dies
- Cleanup of old images post-update
- Defensive correctness for mount/volume dedup and sha256-tag fallback (the subtle bits)
- Emit journal events: `update.checked`, `update.success`, `update.failed`, `update.skipped`

### 9.2 `dashboard` (controller-side)

- Implements `Plugin + HttpHandler`
- Mounts `/` (Atlas SSR), `/host/:id`, `/container/:id`, `/api/v1/*` JSON endpoints
- Serves Leptos WASM hydration bundle from embedded static assets
- Reads inventory and journal directly via `isengard-core` services exposed on `PluginContext`
- WebSocket endpoint `/ws/events` for live journal stream (Atlas comets and pulses)

### 9.3 `notifier` (controller-side)

- Implements `Plugin + EventSubscriber`
- Subscribes to a configurable set of event kinds (default: `update.success`, `update.failed`, `agent.disconnect_long`)
- For each event, fires configurable channels: Slack incoming webhook, Discord webhook, generic HTTP POST, optionally Telegram bot
- Per-channel templates with sensible defaults (one-line summary + link to dashboard event page)
- Rate limit: configurable per channel (default 10 events / minute), batched into a single message when exceeded

## 10. Migration approach

Rewrite, not port. Steps:

1. Port the existing Go test cases first to Rust (translate the behaviors expected, not the implementation). This becomes the spec the new `updater` plugin must satisfy.
2. Implement against the tests. Subtle behaviors — digest extraction, faithful recreation, mount/volume dedup, rename-first self-update — are re-derived by passing the suite, not copy-translated.
3. Existing Go code stays on `main` while the Rust work happens on `feat/platform-rewrite`.
4. When v1 ships and is validated at the friend's prod for a soak window (≥ 1 week), `feat/platform-rewrite` becomes `main`. Go code is removed in the merge commit. Tag the prior `main` as `v0-go` for archival.

Public consequence: 3 stars on the existing repo are not a blocker. README will lead with the platform pivot story; Go-era docs are linked in a "history" section.

## 11. Error handling

- `thiserror` for crate-level error enums; `anyhow` only at binary boundaries (`main`).
- Network and Docker errors are recoverable — log + retry with exponential backoff. Never panic.
- Panic only on invariant violations (e.g., serde of host-internal types failing). Panics in plugin code do not bring down the host: each plugin runs in a `tokio::spawn`-bounded task with a panic catcher; on panic, the plugin is marked unhealthy and a `plugin.crashed` event is journaled.
- Agent-side: agent disconnect is normal. Reconnect with exponential backoff (1s → 60s, jittered).
- Controller-side: agent unreachable for `> 5 min` is journaled as `agent.unreachable`; for `> 4 h` is journaled as `agent.long_unreachable` and surfaces in the dashboard inscription bar.
- No silent failures: every error path either retries (with backoff) or journals.

## 12. Testing strategy

- **Unit tests** per crate, run with `cargo nextest`.
- **Integration harness** in `crates/isengard-tests/`: uses `testcontainers` to spin up real Docker, a controller, two agents, and run end-to-end scenarios:
  - agent enrollment with valid/invalid token
  - container update happy path
  - update with private registry auth
  - faithful recreation across all preservation axes (ports, mounts, networks, labels, restart policy)
  - self-update sequence
  - controller restart preserves inventory and journal
  - notifier fires on event match
  - dashboard renders with seeded inventory
- **Property tests** on the recreate logic (mount/volume dedup, network reattach) using `proptest`.
- **CI** runs unit + integration in parallel (`sccache`, `cargo-nextest`). Property tests run nightly.

## 13. Out of scope (v1)

Defer to v1.x or later specs:

- Plugins: `scheduler`, `metrics`, `rollback`, `hooks`, `dry-run`
- mTLS authentication (v1.x)
- Per-container policy via labels (v1.x — comes with `scheduler`/`hooks`)
- Cosign signature verification (v1.x or v2)
- Trusted-registry allowlist (v1.x)
- WASM plugin loader (v2)
- `rust-sia` and Sia wire migration (separate spec)
- Multi-controller HA (v2)
- Micro-VM substrate / own runtime (v3+)
- Web UI auth / RBAC / OIDC (v2 — only matters once API is exposed externally)

## 14. Implementation phases

Rough sequencing for the multi-week build. Each phase ends in something demonstrable.

| Phase | Scope | Done when |
|---|---|---|
| 0 | Workspace skeleton + CI | `cargo build --workspace` succeeds, CI green, Justfile runs |
| 1 | `isengard-core` + plugin trait + dummy plugin | A no-op plugin loads on agent and controller startup |
| 2 | gRPC: enrollment + sync stream | An agent enrolls, sends heartbeats; controller writes to inventory |
| 3 | `updater` plugin (agent-side) | Existing Watchtower behavior reproduced; integration tests green |
| 4 | Event journal + `notifier` plugin | Update events fire to a webhook |
| 5 | `dashboard` plugin (Leptos + Atlas) | Friend can open a URL and see the fleet rendered |
| 6 | Friend's prod deploy + soak | Running clean for ≥ 1 week at friend's prod |
| 7 | Merge `feat/platform-rewrite` → `main` | v1 tagged, Go era closed |

Phases 1–5 are the multi-week core build. Phase 6 is the validation gate before the merge in phase 7.

## 15. Open questions / future specs

- **`rust-sia`** — separate Weavers project, then Isengard wire migration
- **Rollback design** — healthcheck-driven rollback has subtle failure modes (when does an update "settle"? what if healthcheck flaps?). Deserves its own design pass before implementation.
- **Hooks design** — sandbox model for arbitrary user scripts running on the agent. Shell? gRPC? Webhook only?
- **Multi-controller HA** — needed when the controller becomes a SPOF that can't be tolerated. SQLite WAL replication? Switch to Postgres? Out of scope for v1.
- **Plugin marketplace** — only meaningful once WASM plugins exist. v3+.

## 16. Decision log links

- This spec
- Prior Go-era code on `main`
- Daily note: `weavers-vault/Daily/2026-04-29.md`
- Atlas mockup: `weavers-vault/.superpowers/brainstorm/93385-1777487414/content/atlas-home.html`
