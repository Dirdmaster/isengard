# Phase 4a: Event Plumbing — Proto, Storage, Bus, Core Types

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** All the load-bearing types for events exist, with no producers or consumers wired yet. Proto has the new `Event` message + `AgentMessage::Event` variant. Storage has the `events` table + `Journal` API. Controller has an `EventBus`. Core has finalised `Event` struct + `EventEmitter` + `EventSubscriber` traits.

**Architecture:** Pure plumbing: shape the types so 4b can wire the agent → controller flow without further refactor. No behavioural changes — controller still ignores Event messages on the wire (handler returns `Status::unimplemented` for now); no agent emits anything yet.

**Tech stack:** Adds `chrono = { version = "0.4", features = ["serde"] }` workspace dep (event timestamps need a real DateTime type, not the bare String the storage crate currently uses for `last_seen_at`).

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §3-§6.

---

## Scope

**In:**
- `chrono` workspace dep (with serde feature)
- Proto: add `Event` message + `AgentMessage::event` oneof variant in `crates/isengard-proto/proto/isengard.v1.proto`. Codegen via existing `tonic-build` produces the Rust types.
- Storage: `0002_events.sql` migration creating the `events` table. `Journal::insert(event)` + `Journal::list(filter)` API in a new `crates/isengard-storage/src/journal.rs` module. `Journal::open` + `Journal::open_in_memory` constructors mirroring `Inventory`'s pattern. The journal can share the same SQLite file as the inventory (single DB file per controller).
- Core: finalise the `Event` struct (replacing the v0.1 stub). Add `EventEmitter` async trait. Refine `EventSubscriber` to take `&Event` (the stub took `&str`). Add `events: Option<Arc<dyn EventEmitter>>` to `PluginContext` (None on controller, will be Some on agent in 4b).
- Controller: `EventBus` type wrapping `tokio::sync::broadcast::Sender<Event>` with `subscribe()` + `publish()`. NOT yet wired to the gRPC service.
- Unit tests: Journal insert/list round-trip, EventBus publish-subscribe, Event serialisation, EventEmitter trait shape.

**Out (deferred to 4b–4f):**
- Agent emitting events (4c)
- Controller receiving events from the wire (4b)
- EventBus published from gRPC handler (4b)
- Notifier subscribing (4d)
- agent.disconnect_long (4f)
- Journal pruning / GC (v1.x)
- Event replay on controller restart (v1.x)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 101 baseline + new unit tests
3. `just ci-local` clean
4. Tag `v0.1.0-alpha.phase4a` set locally
5. **Not pushed** until user confirms

---

## File Structure

```
Cargo.toml                                  # MODIFY: add chrono workspace dep

crates/isengard-proto/
├── proto/isengard.v1.proto                 # MODIFY: + Event message, + AgentMessage::event variant
└── (codegen auto-updated)

crates/isengard-storage/
├── Cargo.toml                              # MODIFY: chrono dep
├── migrations/
│   ├── 0001_hosts.sql                      # UNCHANGED
│   └── 0002_events.sql                     # NEW
└── src/
    ├── lib.rs                              # MODIFY: + pub mod journal
    ├── error.rs                            # UNCHANGED
    ├── host.rs                             # UNCHANGED
    ├── inventory.rs                        # UNCHANGED
    └── journal.rs                          # NEW: Journal::open + insert + list

crates/isengard-core/
├── Cargo.toml                              # MODIFY: chrono dep
└── src/
    ├── lib.rs                              # MODIFY: re-export new types
    ├── event.rs                            # MODIFY: replace v0.1 stub with full Event + EventEmitter
    ├── plugin.rs                           # MODIFY: refine EventSubscriber to take &Event
    └── context.rs                          # MODIFY: + events field on PluginContext

crates/isengard-controller/
├── Cargo.toml                              # UNCHANGED (tokio already has broadcast)
└── src/
    ├── lib.rs                              # MODIFY: + pub mod bus
    └── bus.rs                              # NEW: EventBus
```

---

## Task 1: Workspace + crate Cargo.toml — chrono dep

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/isengard-storage/Cargo.toml`
- Modify: `crates/isengard-core/Cargo.toml`

- [ ] **Step 1: Add chrono to workspace deps**

In `Cargo.toml`, append below the existing `futures-util` line:

```toml
# date/time (event timestamps)
chrono = { version = "0.4.39", default-features = false, features = ["serde", "clock"] }
```

- [ ] **Step 2: Add to crate Cargo.tomls**

In `crates/isengard-storage/Cargo.toml` `[dependencies]`:

```toml
chrono.workspace = true
```

In `crates/isengard-core/Cargo.toml` `[dependencies]`:

```toml
chrono.workspace = true
```

- [ ] **Step 3: Build**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-storage/Cargo.toml crates/isengard-core/Cargo.toml
cd ~/Projects/isengard && git commit -m "chore(deps): add chrono for event timestamps"
```

**Self-review checklist:**
- [ ] Build clean
- [ ] `Cargo.lock` staged
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Proto — Event message + AgentMessage variant

**Files:**
- Modify: `crates/isengard-proto/proto/isengard.v1.proto`

- [ ] **Step 1: Add the Event message and the new AgentMessage variant**

Open `crates/isengard-proto/proto/isengard.v1.proto`. Add the new message above the existing `AgentMessage` definition:

```proto
message Event {
  string kind = 1;
  string occurred_at = 2;
  string summary = 3;
  optional string container_name = 4;
  optional string image = 5;
  optional string old_digest = 6;
  optional string new_digest = 7;
  optional string error = 8;
  optional string metadata_json = 9;
}
```

In the existing `AgentMessage` `oneof body { ... }` block, append:

```proto
    Event event = 3;
```

(Bumping the existing tag numbers — verify the existing oneof uses 1 and 2; if those are taken, use 3.)

- [ ] **Step 2: Verify codegen still works**

```bash
cd ~/Projects/isengard && cargo build -p isengard-proto 2>&1 | tail -10
```

Expected: clean. The codegen creates `isengard::v1::Event` and adds the `Event` variant to `AgentMessage::Body`.

- [ ] **Step 3: Build downstream consumers**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller -p isengard-agent 2>&1 | tail -5
```

Existing pattern matches on `AgentMessage::Body` may now be non-exhaustive — adapt by adding `_ => {}` arms to the existing `match` in the controller's Sync handler and agent's stream consumer until 4b adds proper handling. The `_ => {}` is explicitly TEMPORARY for 4a; 4b will make it real.

- [ ] **Step 4: Re-run all tests**

```bash
cd ~/Projects/isengard && cargo test --workspace --no-fail-fast 2>&1 | grep -E "FAILED|^test result" | tail -15
```

Expected: 101+ pass, 0 fail.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-proto/proto/isengard.v1.proto crates/isengard-controller/src/service.rs crates/isengard-agent/src/sync.rs
cd ~/Projects/isengard && git commit -m "feat(proto): + Event message + AgentMessage::event variant"
```

(Stage only files that changed. The `_ => {}` arm fixes may not be needed if the existing matches use `if let` or are already non-exhaustive — only stage what `git status` shows.)

**Self-review checklist:**
- [ ] All tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: Storage — events table + Journal API

**Files:**
- Create: `crates/isengard-storage/migrations/0002_events.sql`
- Create: `crates/isengard-storage/src/journal.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Create the migration**

Create `crates/isengard-storage/migrations/0002_events.sql`:

```sql
CREATE TABLE events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB,
    kind            TEXT NOT NULL,
    container_name  TEXT,
    image           TEXT,
    old_digest      TEXT,
    new_digest      TEXT,
    error           TEXT,
    summary         TEXT NOT NULL,
    metadata_json   TEXT,
    occurred_at     TEXT NOT NULL,
    received_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_events_host_id    ON events(host_id);
CREATE INDEX idx_events_kind       ON events(kind);
CREATE INDEX idx_events_occurred   ON events(occurred_at);
```

- [ ] **Step 2: Create the Journal module**

Create `crates/isengard-storage/src/journal.rs`:

```rust
//! Append-only journal of events emitted by agents (or the controller itself).

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::error::Error;
use crate::host::HostId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub host_id: Option<HostId>,
    pub kind: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata_json: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InsertEvent {
    pub host_id: Option<HostId>,
    pub kind: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata_json: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

pub struct Journal {
    pool: SqlitePool,
}

impl Journal {
    /// Open a journal sharing the given SQLite file. Migrations are run by
    /// the Inventory side; opening Journal assumes the schema exists.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let url = format!("sqlite://{}", path.as_ref().display());
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| Error::Open(e.to_string()))?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|e| Error::Open(e.to_string()))?;
        // Run migrations on the in-memory DB so the events table exists.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| Error::Open(e.to_string()))?;
        Ok(Self { pool })
    }

    pub async fn insert(&self, ev: InsertEvent) -> Result<i64, Error> {
        let host_id_bytes = ev.host_id.as_ref().map(|h| h.as_bytes().to_vec());
        let occurred = ev.occurred_at.to_rfc3339();
        let row = sqlx::query(
            "INSERT INTO events (host_id, kind, container_name, image, old_digest, new_digest, error, summary, metadata_json, occurred_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(host_id_bytes)
        .bind(&ev.kind)
        .bind(&ev.container_name)
        .bind(&ev.image)
        .bind(&ev.old_digest)
        .bind(&ev.new_digest)
        .bind(&ev.error)
        .bind(&ev.summary)
        .bind(&ev.metadata_json)
        .bind(&occurred)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::Decode(e.to_string()))?;
        Ok(row.get::<i64, _>(0))
    }

    /// Most-recent first, capped at `limit`.
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<EventRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, host_id, kind, container_name, image, old_digest, new_digest, error, summary, metadata_json, occurred_at, received_at
             FROM events
             ORDER BY occurred_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Decode(e.to_string()))?;

        rows.into_iter().map(row_to_event).collect()
    }
}

fn row_to_event(row: sqlx::sqlite::SqliteRow) -> Result<EventRow, Error> {
    let host_id_bytes: Option<Vec<u8>> = row.get(1);
    let host_id = match host_id_bytes {
        Some(b) if b.len() == 16 => {
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&b);
            Some(HostId::from_bytes(arr))
        }
        _ => None,
    };
    let occurred_str: String = row.get(10);
    let received_str: String = row.get(11);
    Ok(EventRow {
        id: row.get(0),
        host_id,
        kind: row.get(2),
        container_name: row.get(3),
        image: row.get(4),
        old_digest: row.get(5),
        new_digest: row.get(6),
        error: row.get(7),
        summary: row.get(8),
        metadata_json: row.get(9),
        occurred_at: DateTime::parse_from_rfc3339(&occurred_str)
            .map_err(|e| Error::Decode(format!("bad occurred_at: {e}")))?
            .with_timezone(&Utc),
        received_at: DateTime::parse_from_rfc3339(&received_str)
            .or_else(|_| {
                // SQLite's CURRENT_TIMESTAMP yields "YYYY-MM-DD HH:MM:SS" (UTC, no TZ marker).
                let with_z = format!("{received_str}Z").replace(' ', "T");
                DateTime::parse_from_rfc3339(&with_z)
            })
            .map_err(|e| Error::Decode(format!("bad received_at: {e}")))?
            .with_timezone(&Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(kind: &str) -> InsertEvent {
        InsertEvent {
            host_id: None,
            kind: kind.into(),
            container_name: Some("web".into()),
            image: Some("nginx:1.25".into()),
            old_digest: Some("sha256:aaa".into()),
            new_digest: Some("sha256:bbb".into()),
            error: None,
            summary: format!("{kind} happened"),
            metadata_json: None,
            occurred_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let j = Journal::open_in_memory().await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn insert_then_list_round_trips() {
        let j = Journal::open_in_memory().await.unwrap();
        j.insert(make_event("update.success")).await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "update.success");
        assert_eq!(rows[0].container_name.as_deref(), Some("web"));
        assert_eq!(rows[0].new_digest.as_deref(), Some("sha256:bbb"));
    }

    #[tokio::test]
    async fn list_recent_orders_by_occurred_at_desc() {
        let j = Journal::open_in_memory().await.unwrap();
        let mut e1 = make_event("update.success");
        e1.occurred_at = Utc::now() - chrono::Duration::seconds(10);
        let mut e2 = make_event("update.failed");
        e2.occurred_at = Utc::now();
        j.insert(e1).await.unwrap();
        j.insert(e2).await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert_eq!(rows[0].kind, "update.failed");
        assert_eq!(rows[1].kind, "update.success");
    }

    #[tokio::test]
    async fn list_recent_respects_limit() {
        let j = Journal::open_in_memory().await.unwrap();
        for i in 0..5 {
            j.insert(make_event(&format!("update.kind{i}"))).await.unwrap();
        }
        let rows = j.list_recent(3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn host_id_round_trips_through_sqlite_blob() {
        let j = Journal::open_in_memory().await.unwrap();
        let host = HostId::new();
        let mut ev = make_event("update.checked");
        ev.host_id = Some(host.clone());
        j.insert(ev).await.unwrap();
        let rows = j.list_recent(1).await.unwrap();
        assert_eq!(rows[0].host_id.as_ref(), Some(&host));
    }
}
```

- [ ] **Step 3: Add `pub mod journal;` to lib.rs and re-export**

In `crates/isengard-storage/src/lib.rs`, after the existing `pub mod inventory;`:

```rust
pub mod journal;

pub use journal::{EventRow, InsertEvent, Journal};
```

- [ ] **Step 4: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage 2>&1 | tail -15
```

Expected: 15 (existing) + 5 new = 20 tests pass.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/migrations/0002_events.sql crates/isengard-storage/src/journal.rs crates/isengard-storage/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(storage): events table + Journal::insert/list_recent (5 unit tests)"
```

**Self-review checklist:**
- [ ] All 20 storage tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-storage --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 4: Core — finalised Event + EventEmitter + EventSubscriber

**Files:**
- Modify: `crates/isengard-core/src/event.rs`
- Modify: `crates/isengard-core/src/plugin.rs`
- Modify: `crates/isengard-core/src/context.rs`
- Modify: `crates/isengard-core/src/lib.rs`

- [ ] **Step 1: Replace event.rs with the finalised types**

Read `crates/isengard-core/src/event.rs`. The existing v0.1 stub is something like `pub enum EventKind { ... }` + a thin Event struct. Replace with the full version. Final state:

```rust
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HostId;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Event {
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub host_id: Option<HostId>,
    pub summary: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Async sink for events emitted by plugins.
#[async_trait::async_trait]
pub trait EventEmitter: Send + Sync + 'static {
    async fn emit(&self, event: Event);
}

/// A no-op emitter for contexts where events go nowhere (e.g. unit tests).
pub struct NoopEmitter;

#[async_trait::async_trait]
impl EventEmitter for NoopEmitter {
    async fn emit(&self, _event: Event) {}
}

/// Convenience for plugins: wrap a function/closure into an emitter.
pub fn arc_emitter<E: EventEmitter>(e: E) -> Arc<dyn EventEmitter> {
    Arc::new(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialises_round_trip() {
        let e = Event {
            kind: "update.success".into(),
            occurred_at: Utc::now(),
            summary: "ok".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "update.success");
        assert_eq!(back.summary, "ok");
    }

    #[tokio::test]
    async fn noop_emitter_swallows_event() {
        let e = NoopEmitter;
        e.emit(Event::default()).await;
    }
}
```

**HostId import note:** `HostId` lives in `isengard-storage`, not `isengard-core`. To avoid a circular dep, define a `HostId` re-export shim in core. Two options:
  - **A)** Move `HostId` to core (cleaner long-term, requires migration tweaks in storage)
  - **B)** Define `pub type HostId = [u8; 16];` in core and convert at storage boundary (decouples)

Pick **B** for 4a — minimal blast radius. Update the import:

```rust
use crate::HostId;  // defined as `pub type HostId = ulid::Ulid;` in core
```

Add at the top of `crates/isengard-core/src/lib.rs`:

```rust
pub use ulid::Ulid as HostId;
```

And add `ulid = { workspace = true }` to `crates/isengard-core/Cargo.toml` `[dependencies]`. (`ulid` is already in workspace deps from Phase 2b.)

In `crates/isengard-storage/src/host.rs`, the existing `HostId` type can stay as a wrapper but should expose `From<ulid::Ulid>` and `Into<ulid::Ulid>` so code can pass between core and storage. Verify the current `HostId` shape; if it's already a `ulid::Ulid` newtype, add the conversion:

```rust
impl From<ulid::Ulid> for HostId {
    fn from(u: ulid::Ulid) -> Self { Self(u) }
}

impl From<HostId> for ulid::Ulid {
    fn from(h: HostId) -> Self { h.0 }
}
```

- [ ] **Step 2: Refine EventSubscriber in plugin.rs**

In `crates/isengard-core/src/plugin.rs`, find the existing `EventSubscriber` trait stub. Replace its body:

```rust
#[async_trait::async_trait]
pub trait EventSubscriber: Plugin {
    /// Glob-style kind patterns this subscriber wants. "*" matches everything.
    /// Empty list means subscribe to nothing.
    fn event_kinds(&self) -> &[&'static str];

    /// Handle a single matching event.
    async fn handle(&self, event: &crate::Event) -> crate::Result<()>;
}
```

If the existing stub used a different signature (e.g., `&str` instead of `&Event`), update it. Other plugins that previously implemented the stub (none yet — the Phase 0 dashboard/notifier crates are empty) won't break.

- [ ] **Step 3: Add events field to PluginContext**

In `crates/isengard-core/src/context.rs`, modify `PluginContext`:

```rust
use std::sync::Arc;

use serde_json::Value;

use crate::EventEmitter;

#[derive(Clone)]
pub struct PluginContext {
    pub mode: HostMode,
    pub config: Value,
    pub events: Option<Arc<dyn EventEmitter>>,
}

impl PluginContext {
    pub fn new(mode: HostMode, config: Value) -> Self {
        Self {
            mode,
            config,
            events: None,
        }
    }

    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }
}
```

(`HostMode` is unchanged — it's already in this file.)

The `#[derive(Clone)]` on PluginContext was likely already present; if it was `Debug + Clone`, drop `Debug` because `dyn EventEmitter` doesn't implement Debug. Add a manual Debug impl that elides the events field:

```rust
impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("mode", &self.mode)
            .field("config", &self.config)
            .field("events", &self.events.as_ref().map(|_| "<emitter>"))
            .finish()
    }
}
```

- [ ] **Step 4: Re-export from lib.rs**

In `crates/isengard-core/src/lib.rs`, ensure these are re-exported (alongside what's already exported):

```rust
pub use event::{Event, EventEmitter, NoopEmitter};
pub use plugin::{EventSubscriber, ...};  // keep existing exports
```

- [ ] **Step 5: Build + test**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-core 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: all clean. Existing core tests still pass + new event_serialises_round_trip + noop_emitter_swallows_event.

- [ ] **Step 6: Fix downstream call sites**

`PluginContext::new(mode, config)` already exists, so most call sites work. But the new `events: None` initialiser may need to be added explicitly anywhere PluginContext is built struct-literal-style. Run:

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | grep "error\[E" | head -10
```

Fix any errors by either using `PluginContext::new(...)` (which now defaults events to None) or adding `events: None,` to the struct literal.

- [ ] **Step 7: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-core/Cargo.toml crates/isengard-core/src/event.rs crates/isengard-core/src/plugin.rs crates/isengard-core/src/context.rs crates/isengard-core/src/lib.rs crates/isengard-storage/src/host.rs
cd ~/Projects/isengard && git commit -m "feat(core): finalise Event + EventEmitter + EventSubscriber + PluginContext.events"
```

**Self-review checklist:**
- [ ] All workspace builds clean
- [ ] All tests pass
- [ ] Clippy clean
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 5: Controller — EventBus

**Files:**
- Create: `crates/isengard-controller/src/bus.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

- [ ] **Step 1: Create the EventBus module**

Create `crates/isengard-controller/src/bus.rs`:

```rust
//! In-process event bus. Publishers (the gRPC handler, internal tasks) call
//! `publish`; subscribers (controller-side plugins) call `subscribe` to get
//! a `broadcast::Receiver`.
//!
//! Capacity is generous (1024). Slow subscribers will Lag-and-drop, which
//! is the right semantics — a stuck notifier mustn't backpressure the
//! journal write path.

use std::sync::Arc;

use isengard_core::Event;
use tokio::sync::broadcast;

const BUS_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<broadcast::Sender<Event>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self {
            inner: Arc::new(tx),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.subscribe()
    }

    /// Publish to all current subscribers. Errors mean nobody is listening
    /// — that's normal at startup and we don't surface it.
    pub fn publish(&self, event: Event) {
        let _ = self.inner.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_error() {
        let bus = EventBus::new();
        bus.publish(ev("update.checked"));
        // No assertion — just exercising the path.
    }

    #[tokio::test]
    async fn one_subscriber_receives_published_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(ev("update.success"));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind, "update.success");
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_event() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(ev("update.failed"));
        assert_eq!(rx1.recv().await.unwrap().kind, "update.failed");
        assert_eq!(rx2.recv().await.unwrap().kind, "update.failed");
    }

    #[tokio::test]
    async fn subscriber_count_reflects_active_receivers() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);
        let _r1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        let _r2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_r1);
        assert_eq!(bus.subscriber_count(), 1);
    }
}
```

- [ ] **Step 2: Add `pub mod bus;` to lib.rs**

In `crates/isengard-controller/src/lib.rs`, add after existing module declarations:

```rust
pub mod bus;
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
```

Expected: 4 existing controller unit tests + 4 new bus tests + 4 server_skeleton integration tests = 12 pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/bus.rs crates/isengard-controller/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(controller): EventBus — broadcast publish/subscribe (capacity 1024)"
```

**Self-review checklist:**
- [ ] All controller tests pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-controller --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 6: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4a`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 101 baseline + 5 journal + 2 event + 4 bus = ~112. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4a -m "phase 4a: event plumbing — proto Event + Journal + EventBus + core types"
cd ~/Projects/isengard && git tag -l | grep phase4a
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 101 baseline + new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Tag `v0.1.0-alpha.phase4a` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§3-§6) | Plan task |
|---|---|
| Proto Event + AgentMessage variant | Task 2 |
| `events` table | Task 3 (migration) |
| Journal::insert + list | Task 3 |
| Core Event finalised | Task 4 |
| EventEmitter trait | Task 4 |
| EventSubscriber refined | Task 4 |
| PluginContext.events field | Task 4 |
| EventBus | Task 5 |

No producers or consumers wired — that's 4b–4f. Plumbing only.

**Type consistency check:**
- `chrono::DateTime<Utc>` used uniformly across `Event`, `EventRow`, `InsertEvent`.
- `HostId` aliased to `ulid::Ulid` in core; storage's existing `HostId` newtype gets `From`/`Into` conversions.
- `Arc<dyn EventEmitter>` in PluginContext matches `EventBus`-style trait object pattern.
- `tokio::sync::broadcast::Sender<Event>` — Event is Clone (added via derive), required for broadcast.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4a-events-plumbing.md`. Subagent-driven execution.
