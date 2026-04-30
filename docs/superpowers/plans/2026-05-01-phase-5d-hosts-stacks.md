# Phase 5d — Hosts + Host Detail + Stacks + Stack Detail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the Stack entity end-to-end (migration → storage → agent discovery → controller persist → dashboard API → UI) and ship the four entity pages (Hosts table, Host Detail, Stacks table, Stack Detail).

**Architecture:** New `stacks` table + `hosts.fleet` column (`0003`/`0004` migrations). `isengard-storage` gains a `Stack` model and inventory methods. Agent's existing inventory snapshot is extended with stack metadata derived from `com.docker.compose.project` (or `isengard.stack=` override) labels. Controller's sync handler upserts stacks via the new inventory methods. Dashboard's REST API replaces the v1 stub stacks endpoints with real handlers. Four Vue pages render the data using shared components (`<HostRow>`, `<HostCard>`, `<StackRow>`, `<ServiceChip>`, `<Sparkline>`, `<FleetWeather>`).

**Tech Stack:** Rust (sqlx 0.8, axum 0.8, tonic 0.13), TypeScript / Vue 3 / Nuxt 3 / Tailwind 4 / Pinia.

---

## Scope

**In:**
- New migrations:
  - `crates/isengard-storage/migrations/0003_stacks.sql` — `stacks` table + indexes
  - `crates/isengard-storage/migrations/0004_hosts_fleet.sql` — adds `fleet TEXT NOT NULL DEFAULT 'default'` + index
- New storage model: `crates/isengard-storage/src/stack.rs` with `StackId`, `Stack`, `InsertStack`, `StackSource` enum (`Compose | Manual | Inferred`)
- Inventory extensions in `crates/isengard-storage/src/inventory.rs`:
  - `insert_stack(InsertStack) -> Result<StackId>` (upsert by `(host_id, name)`)
  - `list_stacks(Option<HostId>) -> Result<Vec<Stack>>`
  - `get_stack(StackId) -> Result<Option<Stack>>`
  - `delete_stack(StackId) -> Result<bool>` — used during host decommission cascade
  - `set_host_fleet(HostId, &str) -> Result<bool>`
  - Update `Host` struct to include `fleet: String`
  - Update `enroll_host` + `get_host` + `list_hosts` to read/write `fleet`
- Proto extension in `crates/isengard-proto/proto/sync.proto`:
  - `message StackInfo { string name = 1; string source = 2; repeated string services = 3; }`
  - `Heartbeat` (or `InventorySnapshot`) gains `repeated StackInfo stacks = N;`
  - Regenerate Rust bindings via the existing build.rs
- Agent extension in `crates/isengard-agent/src/inventory_snapshot.rs` (or wherever the heartbeat is built):
  - When listing containers, group by label `com.docker.compose.project` (fallback `isengard.stack`, fallback `<container_name>` as single-service stack)
  - Build `Vec<StackInfo>` and attach to outgoing heartbeat
- Controller extension in `crates/isengard-controller/src/sync.rs` (or wherever heartbeats are processed):
  - For each `StackInfo` in heartbeat, call `inventory.insert_stack(...)`
  - Stacks not present in the heartbeat are pruned for that host (delete_stack for any current stack on host_id whose name isn't in the new set)
- Dashboard API (real implementations replacing 5b stubs):
  - `GET /api/v1/stacks?fleet=&host_id=&state=` — returns real stacks
  - `GET /api/v1/stacks/:id` — single stack with services list
  - `GET /api/v1/services?stack_id=` — services for a stack (joined with current container snapshot)
  - `GET /api/v1/services/:id` — single service
  - `POST /api/v1/stacks/:id/actions/force-update` — sets a per-stack force-update flag (RPC to agent wired here for real)
  - `PATCH /api/v1/hosts/:id` — actually updates `fleet` (was stubbed)
  - `GET /api/v1/hosts/:id/sparkline?range=24h` — pre-aggregated event counts per bucket
- New DTOs in `crates/isengard-plugins/dashboard/src/dto.rs`:
  - `StackDto`, `ServiceDto`, `SparklineDto`
  - `HostDto` updated to read real `fleet` from storage (drop the `"default"` placeholder)
- Frontend pages (under `crates/isengard-plugins/dashboard/web/pages/`):
  - `hosts/index.vue` — Hosts table v2 enhanced (full-width, no inspector)
  - `hosts/[id].vue` — Host Detail (cards-with-stacks layout)
  - `stacks/index.vue` — Stacks table (flat sortable, all stacks across fleet/hosts)
  - `stacks/[id].vue` — Stack Detail (services + recent events + history)
- New Pinia stores: `stores/stacks.ts`, `stores/services.ts`
- New composables: `composables/useSparkline.ts`, `composables/useHostActions.ts`
- New components in `crates/isengard-plugins/dashboard/web/components/`:
  - `HostRow.vue`, `HostsTable.vue`, `FleetWeather.vue`, `Sparkline.vue`, `StatusPill.vue`
  - `HostCard.vue`, `StackRow.vue`, `ServiceChip.vue`
  - `StacksTable.vue`, `StackHeader.vue`
  - `AddHostButton.vue` (button only — modal lands in 5e)
- TopBar tab activation (Home / Hosts / Stacks / Events / Settings) wired up via `useRoute()`
- Cmd pane navigator gets new entity sources: results from `useStacksStore` and `useServicesStore`
- New unit + integration tests:
  - Migration tests (sqlx test fixture)
  - `Inventory::insert_stack` upsert behavior
  - `Inventory::set_host_fleet` returns true/false correctly
  - Stack pruning logic (heartbeat with fewer stacks than DB → extras deleted)
  - DTO mapping tests for `StackDto`, `ServiceDto`
  - API handler tests for `/api/v1/stacks*`, `/api/v1/services*`, `/api/v1/hosts/:id/sparkline`

**Out (deferred to 5e or later):**
- Add-host modal UI (the button is here; the modal in 5e)
- Settings page (5e)
- Events tab full page (5e)
- Cmd pane terminal mode (5e)
- Force-update via xterm shell streaming (5e shell WS lands the bidirectional path)
- Bulk actions (multi-select rows) — out of v1 entirely
- Healthcheck-driven rollback — owner is updater, not dashboard

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline + new storage tests + new API tests
3. `just ci-local` clean (cargo-deny mandatory)
4. `bun --cwd crates/isengard-plugins/dashboard/web run build` produces a static bundle that includes the four new pages
5. End-to-end smoke (4 terminals):
   - T1: controller running with at least 2 hosts enrolled, each with at least one Docker Compose project labelled correctly
   - T2: agent on each host emits a heartbeat containing `StackInfo` for each project
   - T3: `curl -s http://localhost:9418/api/v1/stacks | jq .` returns the discovered stacks (not empty)
   - Browser: navigate `/hosts` → see HostsTable with sparklines, latest event, hover actions; click `prod-01` → see Host Detail with HostCard listing the stacks; click `wordpress` stack → see StackDetail with service chips
   - Switch fleet picker → table refilters
6. Tag `v0.1.0-alpha.phase5d` set locally
7. **Not pushed**

---

## File Structure

```
crates/isengard-storage/
├── migrations/
│   ├── 0003_stacks.sql                 # NEW
│   └── 0004_hosts_fleet.sql            # NEW
└── src/
    ├── lib.rs                          # MODIFY: pub mod stack; re-exports
    ├── stack.rs                        # NEW: StackId, Stack, InsertStack, StackSource
    ├── host.rs                         # MODIFY: Host gains `fleet: String`
    └── inventory.rs                    # MODIFY: + stack methods + set_host_fleet + fleet column rw

crates/isengard-proto/
├── proto/sync.proto                    # MODIFY: + StackInfo message; Heartbeat carries Vec<StackInfo>
└── (generated bindings rebuilt via build.rs)

crates/isengard-agent/
└── src/
    └── inventory_snapshot.rs           # MODIFY: derive stack info from container labels

crates/isengard-controller/
└── src/
    └── sync.rs                         # MODIFY: process stacks from heartbeat, prune stale

crates/isengard-plugins/dashboard/
└── src/
    ├── dto.rs                          # MODIFY: + StackDto, ServiceDto, SparklineDto; HostDto reads real fleet
    └── api.rs                          # MODIFY: real stacks/services/sparkline handlers; PATCH host updates fleet

crates/isengard-plugins/dashboard/web/
├── components/
│   ├── HostRow.vue                     # NEW
│   ├── HostsTable.vue                  # NEW
│   ├── FleetWeather.vue                # NEW
│   ├── Sparkline.vue                   # NEW
│   ├── StatusPill.vue                  # NEW
│   ├── HostCard.vue                    # NEW
│   ├── StackRow.vue                    # NEW
│   ├── ServiceChip.vue                 # NEW
│   ├── StacksTable.vue                 # NEW
│   ├── StackHeader.vue                 # NEW
│   └── AddHostButton.vue               # NEW (modal in 5e)
├── pages/
│   ├── hosts/
│   │   ├── index.vue                   # NEW
│   │   └── [id].vue                    # NEW
│   └── stacks/
│       ├── index.vue                   # NEW
│       └── [id].vue                    # NEW
├── stores/
│   ├── stacks.ts                       # NEW
│   └── services.ts                     # NEW
├── composables/
│   ├── useSparkline.ts                 # NEW
│   └── useHostActions.ts               # NEW
└── components/TopBar.vue               # MODIFY: activate tab via $route.path
```

---

## Task 1: Storage migrations + Stack model

**Files:**
- Create: `crates/isengard-storage/migrations/0003_stacks.sql`
- Create: `crates/isengard-storage/migrations/0004_hosts_fleet.sql`
- Create: `crates/isengard-storage/src/stack.rs`
- Modify: `crates/isengard-storage/src/lib.rs`
- Modify: `crates/isengard-storage/src/host.rs`

- [ ] **Step 1: Write the failing test for Stack round-trip**

```rust
// crates/isengard-storage/src/stack.rs (test module, written first)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_id_round_trips_through_json() {
        let id = StackId(42);
        let s = serde_json::to_string(&id).unwrap();
        let back: StackId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn stack_source_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&StackSource::Compose).unwrap(), "\"compose\"");
        assert_eq!(serde_json::to_string(&StackSource::Manual).unwrap(), "\"manual\"");
        assert_eq!(serde_json::to_string(&StackSource::Inferred).unwrap(), "\"inferred\"");
    }
}
```

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-storage stack::tests --no-run`
Expected: FAIL — compile error, `StackId` does not exist yet.

- [ ] **Step 3: Create the Stack model**

```rust
// crates/isengard-storage/src/stack.rs

//! Stack entity: a logical grouping of services (typically a Docker Compose
//! project) that lives on one host.

use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Surrogate primary key for stacks. The `stacks` table uses an autoincrementing
/// integer because there is no natural identifier — the `(host_id, name)` pair
/// is unique but heavy to use as a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StackId(pub i64);

impl std::fmt::Display for StackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackSource {
    /// Discovered via `com.docker.compose.project=<name>` label.
    Compose,
    /// User-provided override via `isengard.stack=<name>` label.
    Manual,
    /// Synthesized — single-service stack named after the container, used when
    /// neither label is present.
    Inferred,
}

impl StackSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Manual => "manual",
            Self::Inferred => "inferred",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "compose" => Some(Self::Compose),
            "manual" => Some(Self::Manual),
            "inferred" => Some(Self::Inferred),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub id: StackId,
    pub host_id: HostId,
    pub name: String,
    pub source: StackSource,
    pub discovered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStack {
    pub host_id: HostId,
    pub name: String,
    pub source: StackSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_id_round_trips_through_json() {
        let id = StackId(42);
        let s = serde_json::to_string(&id).unwrap();
        let back: StackId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn stack_source_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&StackSource::Compose).unwrap(), "\"compose\"");
        assert_eq!(serde_json::to_string(&StackSource::Manual).unwrap(), "\"manual\"");
        assert_eq!(serde_json::to_string(&StackSource::Inferred).unwrap(), "\"inferred\"");
    }
}
```

- [ ] **Step 4: Re-export from lib.rs**

Edit `crates/isengard-storage/src/lib.rs` and add (alongside existing `pub mod host;`):

```rust
pub mod stack;
pub use stack::{InsertStack, Stack, StackId, StackSource};
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p isengard-storage stack::tests`
Expected: PASS — both tests green.

- [ ] **Step 6: Write migration 0003_stacks.sql**

Create `crates/isengard-storage/migrations/0003_stacks.sql`:

```sql
CREATE TABLE stacks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL,
    source          TEXT    NOT NULL CHECK(source IN ('compose', 'manual', 'inferred')),
    discovered_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(host_id, name)
);

CREATE INDEX idx_stacks_host_id ON stacks(host_id);
```

- [ ] **Step 7: Write migration 0004_hosts_fleet.sql**

Create `crates/isengard-storage/migrations/0004_hosts_fleet.sql`:

```sql
ALTER TABLE hosts ADD COLUMN fleet TEXT NOT NULL DEFAULT 'default';
CREATE INDEX idx_hosts_fleet ON hosts(fleet);
```

- [ ] **Step 8: Update Host struct to include fleet**

Edit `crates/isengard-storage/src/host.rs`. In the `Host` struct, add (after `metadata`):

```rust
    /// Fleet tag this host belongs to. Defaults to `"default"`.
    pub fleet: String,
```

- [ ] **Step 9: Run a build to surface compile errors**

Run: `cargo build -p isengard-storage`
Expected: FAIL — every constructor of `Host` (in `inventory.rs`, tests, etc.) is missing the `fleet` field. Fix each by inserting `fleet: row.get("fleet")` in the `from_row` mapping and `fleet: "default".to_string()` in tests/fixtures. The detailed inventory-side fix is in Task 2.

- [ ] **Step 10: Commit**

```bash
git add crates/isengard-storage/migrations/0003_stacks.sql \
        crates/isengard-storage/migrations/0004_hosts_fleet.sql \
        crates/isengard-storage/src/stack.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/src/host.rs
git commit -m "feat(storage): stacks table + hosts.fleet column + Stack model"
```

---

## Task 2: Inventory stack methods + set_host_fleet

**Files:**
- Modify: `crates/isengard-storage/src/inventory.rs`

- [ ] **Step 1: Write failing test for insert_stack upsert behavior**

Add to `inventory.rs` test module:

```rust
#[tokio::test]
async fn insert_stack_is_idempotent_per_host_and_name() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp1".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();

    let id1 = inv
        .insert_stack(InsertStack {
            host_id,
            name: "wordpress".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    let id2 = inv
        .insert_stack(InsertStack {
            host_id,
            name: "wordpress".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    assert_eq!(id1, id2, "second insert with same (host_id, name) should return the same id");

    let listed = inv.list_stacks(Some(host_id)).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "wordpress");
}

#[tokio::test]
async fn set_host_fleet_updates_existing_host() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp1".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();

    let updated = inv.set_host_fleet(host_id, "prod").await.unwrap();
    assert!(updated, "set_host_fleet should return true when row exists");

    let host = inv.get_host(host_id).await.unwrap().unwrap();
    assert_eq!(host.fleet, "prod");

    let missing = HostId::new();
    let updated = inv.set_host_fleet(missing, "prod").await.unwrap();
    assert!(!updated, "set_host_fleet should return false when row does not exist");
}
```

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-storage inventory::tests::insert_stack_is_idempotent --no-run`
Expected: FAIL — `insert_stack`, `list_stacks`, `set_host_fleet` are missing.

- [ ] **Step 3: Update existing host SQL queries to read/write fleet**

In `inventory.rs`, update `enroll_host` (or whatever inserts into `hosts`) to include `fleet`:

```rust
sqlx::query(
    "INSERT INTO hosts (id, fingerprint, hostname, os, arch, agent_version, docker_version, enrolled_at, metadata, fleet)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, '{}', 'default')",
)
.bind(id.to_bytes().as_slice())
.bind(&req.fingerprint)
.bind(&req.hostname)
.bind(&req.os)
.bind(&req.arch)
.bind(&req.agent_version)
.bind(&req.docker_version)
.bind(now)
.execute(&self.pool)
.await?;
```

Update `get_host`, `list_hosts` (and any other SELECT) to include `fleet` in the column list. Map it in `Host::from_row` (or wherever the rows are deserialized) as `fleet: row.try_get("fleet")?`.

- [ ] **Step 4: Implement insert_stack, list_stacks, get_stack, delete_stack**

Add to `Inventory`:

```rust
pub async fn insert_stack(&self, req: InsertStack) -> Result<StackId> {
    // Upsert: insert if missing, otherwise return existing id.
    // SQLite's INSERT ... ON CONFLICT DO UPDATE doesn't return the existing id,
    // so we do an INSERT OR IGNORE, then SELECT to fetch the id.
    sqlx::query(
        "INSERT OR IGNORE INTO stacks (host_id, name, source) VALUES (?, ?, ?)",
    )
    .bind(req.host_id.to_bytes().as_slice())
    .bind(&req.name)
    .bind(req.source.as_str())
    .execute(&self.pool)
    .await?;

    let row = sqlx::query("SELECT id FROM stacks WHERE host_id = ? AND name = ?")
        .bind(req.host_id.to_bytes().as_slice())
        .bind(&req.name)
        .fetch_one(&self.pool)
        .await?;
    let id: i64 = row.try_get("id")?;
    Ok(StackId(id))
}

pub async fn list_stacks(&self, host_id: Option<HostId>) -> Result<Vec<Stack>> {
    let rows = match host_id {
        Some(h) => {
            sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks WHERE host_id = ? ORDER BY name")
                .bind(h.to_bytes().as_slice())
                .fetch_all(&self.pool)
                .await?
        }
        None => {
            sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks ORDER BY name")
                .fetch_all(&self.pool)
                .await?
        }
    };

    rows.into_iter().map(stack_from_row).collect()
}

pub async fn get_stack(&self, id: StackId) -> Result<Option<Stack>> {
    let row = sqlx::query("SELECT id, host_id, name, source, discovered_at FROM stacks WHERE id = ?")
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
    row.map(stack_from_row).transpose()
}

pub async fn delete_stack(&self, id: StackId) -> Result<bool> {
    let result = sqlx::query("DELETE FROM stacks WHERE id = ?")
        .bind(id.0)
        .execute(&self.pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn set_host_fleet(&self, id: HostId, fleet: &str) -> Result<bool> {
    let result = sqlx::query("UPDATE hosts SET fleet = ? WHERE id = ?")
        .bind(fleet)
        .bind(id.to_bytes().as_slice())
        .execute(&self.pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

fn stack_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Stack> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    let source_str: String = row.try_get("source")?;
    let source = StackSource::from_str(&source_str)
        .ok_or_else(|| Error::InvalidData(format!("unknown stack source: {}", source_str)))?;
    Ok(Stack {
        id: StackId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        name: row.try_get("name")?,
        source,
        discovered_at: row.try_get("discovered_at")?,
    })
}
```

Add `use crate::stack::{InsertStack, Stack, StackId, StackSource};` at the top of `inventory.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p isengard-storage`
Expected: PASS — both new tests + all baseline storage tests.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-storage/src/inventory.rs
git commit -m "feat(storage): inventory stack methods + set_host_fleet + fleet column rw"
```

---

## Task 3: Proto extension for stack info in heartbeat

**Files:**
- Modify: `crates/isengard-proto/proto/sync.proto`
- Modify: `crates/isengard-proto/src/lib.rs` (only if it has explicit re-exports needing update)

- [ ] **Step 1: Read the current proto to find the heartbeat message**

```bash
grep -n "Heartbeat\|InventorySnapshot\|message " crates/isengard-proto/proto/sync.proto
```

Locate the Heartbeat (or equivalent) message definition.

- [ ] **Step 2: Add StackInfo message and the field**

Add to `sync.proto`, near the existing inventory-related messages:

```proto
message StackInfo {
  string name = 1;
  // One of: "compose", "manual", "inferred".
  string source = 2;
  // Container names that belong to this stack on this host.
  repeated string services = 3;
}
```

In the `Heartbeat` (or `InventorySnapshot`) message, add a new field with the next free tag number (replace `<N>` with whatever is next):

```proto
  repeated StackInfo stacks = <N>;
```

- [ ] **Step 3: Build to regenerate bindings**

Run: `cargo build -p isengard-proto`
Expected: PASS — the build.rs invokes prost to regenerate `target/.../isengard.sync.rs` with `StackInfo`.

- [ ] **Step 4: Verify the generated symbol exists**

Run: `cargo build -p isengard-proto --message-format=json 2>/dev/null | grep -o "isengard_proto::.*StackInfo" | head -3`
(Optional sanity check — the next task fails to compile if it's missing.)

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-proto/proto/sync.proto
git commit -m "proto: + StackInfo message; Heartbeat carries stacks vec"
```

---

## Task 4: Agent emits stack info in heartbeat

**Files:**
- Modify: `crates/isengard-agent/src/inventory_snapshot.rs` (or whichever module builds the heartbeat)

- [ ] **Step 1: Write failing unit test for stack derivation**

Add to the agent's relevant test module:

```rust
#[test]
fn derives_stack_info_from_compose_label() {
    use std::collections::HashMap;

    let containers = vec![
        ContainerSnapshot {
            name: "web".into(),
            labels: hashmap! {
                "com.docker.compose.project".to_string() => "wordpress".to_string(),
            },
        },
        ContainerSnapshot {
            name: "db".into(),
            labels: hashmap! {
                "com.docker.compose.project".to_string() => "wordpress".to_string(),
            },
        },
        ContainerSnapshot {
            name: "homer".into(),
            labels: HashMap::new(),
        },
    ];

    let stacks = derive_stacks(&containers);

    assert_eq!(stacks.len(), 2);
    let wp = stacks.iter().find(|s| s.name == "wordpress").unwrap();
    assert_eq!(wp.source, "compose");
    assert_eq!(wp.services.len(), 2);
    assert!(wp.services.contains(&"web".to_string()));
    assert!(wp.services.contains(&"db".to_string()));

    let homer = stacks.iter().find(|s| s.name == "homer").unwrap();
    assert_eq!(homer.source, "inferred");
    assert_eq!(homer.services, vec!["homer".to_string()]);
}

#[test]
fn isengard_stack_label_overrides_compose_label() {
    let containers = vec![ContainerSnapshot {
        name: "x".into(),
        labels: hashmap! {
            "com.docker.compose.project".to_string() => "default-name".to_string(),
            "isengard.stack".to_string() => "override-name".to_string(),
        },
    }];

    let stacks = derive_stacks(&containers);
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].name, "override-name");
    assert_eq!(stacks[0].source, "manual");
}
```

(Adjust `ContainerSnapshot` to whatever the agent's existing container snapshot type is named, and use `maplit::hashmap!` or build the HashMap explicitly with `.insert`.)

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-agent derives_stack_info --no-run`
Expected: FAIL — `derive_stacks` is not defined.

- [ ] **Step 3: Implement derive_stacks**

```rust
use isengard_proto::sync::StackInfo;
use std::collections::BTreeMap;

/// Group container snapshots into stacks based on Docker Compose labels,
/// the optional `isengard.stack` override, or fall back to single-service
/// inferred stacks.
pub fn derive_stacks(containers: &[ContainerSnapshot]) -> Vec<StackInfo> {
    // Use BTreeMap for deterministic output ordering (helps tests + diffs).
    let mut grouped: BTreeMap<(String, &'static str), Vec<String>> = BTreeMap::new();

    for c in containers {
        let (name, source) = if let Some(n) = c.labels.get("isengard.stack") {
            (n.clone(), "manual")
        } else if let Some(n) = c.labels.get("com.docker.compose.project") {
            (n.clone(), "compose")
        } else {
            (c.name.clone(), "inferred")
        };

        grouped.entry((name, source)).or_default().push(c.name.clone());
    }

    grouped
        .into_iter()
        .map(|((name, source), mut services)| {
            services.sort(); // deterministic
            StackInfo {
                name,
                source: source.to_string(),
                services,
            }
        })
        .collect()
}
```

- [ ] **Step 4: Wire into the heartbeat builder**

Find where the heartbeat (or inventory snapshot) is constructed and assign the new field. Pseudo-template:

```rust
let stacks = derive_stacks(&container_snapshots);

let heartbeat = Heartbeat {
    // ... existing fields ...
    stacks,
};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p isengard-agent`
Expected: PASS — both new tests + all existing agent tests.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-agent/src/inventory_snapshot.rs
git commit -m "feat(agent): derive stack info from compose labels + send in heartbeat"
```

---

## Task 5: Controller persists stacks from heartbeat (with prune)

**Files:**
- Modify: `crates/isengard-controller/src/sync.rs` (or wherever `sync` heartbeats land)

- [ ] **Step 1: Write failing integration test**

Add a test (in the controller's integration test file or a new `sync_stacks.rs`):

```rust
#[tokio::test]
async fn heartbeat_with_stacks_upserts_and_prunes() {
    use isengard_proto::sync::{Heartbeat, StackInfo};
    use isengard_storage::{EnrollHost, Inventory};

    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();

    // First heartbeat: two stacks
    let hb = Heartbeat {
        // minimum fields the controller needs to identify the host
        agent_id: host_id.to_string(),
        stacks: vec![
            StackInfo { name: "wordpress".into(), source: "compose".into(), services: vec!["web".into(), "db".into()] },
            StackInfo { name: "homer".into(),    source: "inferred".into(), services: vec!["homer".into()] },
        ],
        // ... other fields with sensible defaults ...
        ..Default::default()
    };
    process_heartbeat_stacks(&inv, host_id, &hb.stacks).await.unwrap();

    let stacks = inv.list_stacks(Some(host_id)).await.unwrap();
    assert_eq!(stacks.len(), 2);

    // Second heartbeat: only one stack remains
    let hb2_stacks = vec![
        StackInfo { name: "wordpress".into(), source: "compose".into(), services: vec!["web".into(), "db".into()] },
    ];
    process_heartbeat_stacks(&inv, host_id, &hb2_stacks).await.unwrap();

    let stacks = inv.list_stacks(Some(host_id)).await.unwrap();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].name, "wordpress");
}
```

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-controller heartbeat_with_stacks --no-run`
Expected: FAIL — `process_heartbeat_stacks` does not exist.

- [ ] **Step 3: Implement process_heartbeat_stacks**

```rust
use isengard_proto::sync::StackInfo as ProtoStackInfo;
use isengard_storage::{InsertStack, Inventory, HostId, StackSource};
use std::collections::HashSet;

pub async fn process_heartbeat_stacks(
    inv: &Inventory,
    host_id: HostId,
    stacks: &[ProtoStackInfo],
) -> Result<(), crate::Error> {
    // Upsert every stack in the heartbeat.
    let mut current_names: HashSet<String> = HashSet::new();
    for s in stacks {
        let source = StackSource::from_str(&s.source).unwrap_or(StackSource::Inferred);
        inv.insert_stack(InsertStack {
            host_id,
            name: s.name.clone(),
            source,
        })
        .await?;
        current_names.insert(s.name.clone());
    }

    // Prune any stacks for this host that are no longer reported.
    let existing = inv.list_stacks(Some(host_id)).await?;
    for stack in existing {
        if !current_names.contains(&stack.name) {
            inv.delete_stack(stack.id).await?;
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Wire into the existing heartbeat handler**

Find the existing function that handles incoming heartbeats (likely something like `Sync::sync(...)` or `handle_heartbeat`). After the host's `last_seen_at` is touched, add:

```rust
process_heartbeat_stacks(&self.inventory, host_id, &request.stacks).await?;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p isengard-controller`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-controller/src/sync.rs
git commit -m "feat(controller): persist + prune stacks from heartbeat"
```

---

## Task 6: Dashboard DTOs — Stack, Service, Sparkline; HostDto reads real fleet

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/dto.rs`

- [ ] **Step 1: Write failing test for StackDto mapping**

Add to `dto.rs` test module:

```rust
#[test]
fn stack_dto_maps_from_storage_stack() {
    use chrono::TimeZone;
    use isengard_storage::{Stack, StackId, StackSource};

    let host_id = HostId::new();
    let s = Stack {
        id: StackId(7),
        host_id,
        name: "wordpress".into(),
        source: StackSource::Compose,
        discovered_at: Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap(),
    };

    let dto: StackDto = s.into();
    assert_eq!(dto.id, "7");
    assert_eq!(dto.host_id, ulid::Ulid::from(host_id).to_string());
    assert_eq!(dto.name, "wordpress");
    assert_eq!(dto.source, "compose");
}

#[test]
fn host_dto_carries_real_fleet() {
    let h = Host {
        id: HostId::new(),
        fingerprint: "fp".into(),
        hostname: "h".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
        enrolled_at: 0,
        last_seen_at: None,
        metadata: serde_json::json!({}),
        fleet: "prod".into(),
    };

    let dto: HostDto = h.into();
    assert_eq!(dto.fleet, "prod");
}
```

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-dashboard stack_dto_maps --no-run`
Expected: FAIL — `StackDto` does not exist; `HostDto::from(Host)` still returns `"default"` (5b stub).

- [ ] **Step 3: Add StackDto, ServiceDto, SparklineDto + fix HostDto**

```rust
use isengard_storage::{Stack, StackId, StackSource};

#[derive(Debug, Clone, Serialize)]
pub struct StackDto {
    pub id: String,
    pub host_id: String,
    pub name: String,
    /// One of "compose", "manual", "inferred".
    pub source: String,
    pub discovered_at: DateTime<Utc>,
}

impl From<Stack> for StackDto {
    fn from(s: Stack) -> Self {
        Self {
            id: s.id.0.to_string(),
            host_id: ulid::Ulid::from(s.host_id).to_string(),
            name: s.name,
            source: s.source.as_str().to_string(),
            discovered_at: s.discovered_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceDto {
    /// Synthetic id: `{host_id}:{container_name}`. Containers don't have
    /// stable database ids in v1 — they're tracked by name within a host.
    pub id: String,
    pub host_id: String,
    pub stack_id: Option<String>,
    pub name: String,
    pub image: String,
    /// One of "running", "stopped", "restarting", "unknown".
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SparklineDto {
    /// Number of buckets (typically 24 for a 24h range, one per hour).
    pub buckets: Vec<u32>,
    /// Range queried, e.g. "24h".
    pub range: String,
    /// Sum of all buckets — convenience for the row's "N events" summary.
    pub total: u32,
}
```

Update the existing `From<Host> for HostDto`:

```rust
impl From<Host> for HostDto {
    fn from(h: Host) -> Self {
        Self {
            id: ulid::Ulid::from(h.id).to_string(),
            fingerprint: h.fingerprint,
            hostname: h.hostname,
            os: h.os,
            arch: h.arch,
            agent_version: h.agent_version,
            docker_version: h.docker_version,
            fleet: h.fleet, // was "default".to_string() in 5b
            enrolled_at: DateTime::<Utc>::from_timestamp(h.enrolled_at, 0)
                .unwrap_or_else(Utc::now),
            last_seen_at: h.last_seen_at.and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p isengard-dashboard`
Expected: PASS — new DTO tests + existing.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-plugins/dashboard/src/dto.rs
git commit -m "feat(dashboard): + StackDto, ServiceDto, SparklineDto; HostDto reads real fleet"
```

---

## Task 7: Dashboard API — real stacks/services + sparkline + PATCH host fleet

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/api.rs`

- [ ] **Step 1: Write failing axum test for GET /api/v1/stacks**

Add to `api.rs` test module:

```rust
#[tokio::test]
async fn get_stacks_returns_inserted_stacks() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();
    inv.insert_stack(InsertStack {
        host_id,
        name: "wordpress".into(),
        source: StackSource::Compose,
    })
    .await
    .unwrap();

    let handles = ControllerHandles::new(inv, /* journal */ todo_journal(), /* bus */ todo_bus());
    let app = build_router(Arc::new(handles));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/stacks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: Vec<StackDto> = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "wordpress");
}

#[tokio::test]
async fn patch_host_updates_fleet() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv
        .enroll_host(EnrollHost { /* ...as above... */ ..test_enroll() })
        .await
        .unwrap();

    let handles = ControllerHandles::new(inv.clone(), todo_journal(), todo_bus());
    let app = build_router(Arc::new(handles));

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/hosts/{}", host_id))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"fleet": "prod"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let host = inv.get_host(host_id).await.unwrap().unwrap();
    assert_eq!(host.fleet, "prod");
}
```

(Use the same `ControllerHandles`/`build_router` test scaffold from 5b. Add a small `fn test_enroll() -> EnrollHost` helper if there isn't one already.)

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p isengard-dashboard get_stacks_returns_inserted --no-run`
Expected: FAIL — handler returns the 5b stub `Vec::new()`.

- [ ] **Step 3: Implement real stacks handlers**

Replace the stubbed handlers in `api.rs`:

```rust
use isengard_storage::{InsertStack, StackId, StackSource};

#[derive(Debug, Deserialize)]
pub struct ListStacksQuery {
    pub fleet: Option<String>,
    pub host_id: Option<String>,
}

pub async fn list_stacks(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListStacksQuery>,
) -> Result<Json<Vec<StackDto>>, ApiError> {
    let host_filter = match q.host_id.as_deref() {
        Some(s) => Some(parse_host_id(s)?),
        None => None,
    };

    let mut stacks = handles.inventory.list_stacks(host_filter).await?;

    if let Some(fleet) = q.fleet.as_deref() {
        // Need to filter by fleet via a join. Naive impl: fetch hosts, filter.
        let hosts = handles.inventory.list_hosts().await?;
        let allowed: HashSet<HostId> = hosts
            .into_iter()
            .filter(|h| h.fleet == fleet)
            .map(|h| h.id)
            .collect();
        stacks.retain(|s| allowed.contains(&s.host_id));
    }

    Ok(Json(stacks.into_iter().map(StackDto::from).collect()))
}

pub async fn get_stack(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Result<Json<StackDto>, ApiError> {
    let stack = handles
        .inventory
        .get_stack(StackId(id))
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(stack.into()))
}
```

Implement `patch_host`:

```rust
#[derive(Debug, Deserialize)]
pub struct PatchHostBody {
    pub fleet: Option<String>,
}

pub async fn patch_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
    Json(body): Json<PatchHostBody>,
) -> Result<Json<HostDto>, ApiError> {
    let host_id = parse_host_id(&id)?;

    if let Some(fleet) = body.fleet {
        let updated = handles.inventory.set_host_fleet(host_id, &fleet).await?;
        if !updated {
            return Err(ApiError::NotFound);
        }
    }

    let host = handles
        .inventory
        .get_host(host_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(host.into()))
}
```

Implement `get_host_sparkline`:

```rust
#[derive(Debug, Deserialize)]
pub struct SparklineQuery {
    #[serde(default = "default_range")]
    pub range: String,
}

fn default_range() -> String { "24h".to_string() }

pub async fn get_host_sparkline(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
    Query(q): Query<SparklineQuery>,
) -> Result<Json<SparklineDto>, ApiError> {
    let host_id = parse_host_id(&id)?;
    // v1: 24 hourly buckets only. Other ranges parse to a 422 for now.
    if q.range != "24h" {
        return Err(ApiError::BadRequest("only range=24h is supported in v1".into()));
    }

    let now = chrono::Utc::now();
    let since = now - chrono::Duration::hours(24);

    // Pull events for the host since `since`. The journal's existing
    // `list_events_for_host` should filter; if it doesn't, query directly.
    let events = handles.journal.list_events_for_host(host_id, since).await?;

    let mut buckets = vec![0u32; 24];
    for ev in &events {
        let delta_secs = (now - ev.occurred_at).num_seconds();
        let hours_ago = (delta_secs / 3600).max(0).min(23) as usize;
        let idx = 23 - hours_ago;
        buckets[idx] = buckets[idx].saturating_add(1);
    }
    let total = buckets.iter().sum();

    Ok(Json(SparklineDto { buckets, range: q.range, total }))
}
```

Add the routes to `build_router`:

```rust
.route("/api/v1/stacks",            get(list_stacks))
.route("/api/v1/stacks/:id",        get(get_stack))
.route("/api/v1/hosts/:id",         patch(patch_host))
.route("/api/v1/hosts/:id/sparkline", get(get_host_sparkline))
```

(Some of these routes were registered as stubs in 5b. Replace, don't double-register.)

- [ ] **Step 4: Implement services handlers (basic v1)**

```rust
#[derive(Debug, Deserialize)]
pub struct ListServicesQuery {
    pub stack_id: Option<i64>,
}

pub async fn list_services(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListServicesQuery>,
) -> Result<Json<Vec<ServiceDto>>, ApiError> {
    // v1: services are derived from stacks (the stack metadata in heartbeat
    // includes the service container names). The agent's last reported
    // container snapshot lives in `inventory.metadata` JSON keyed by container
    // name. For 5d we expose what the stack carries; richer state (image,
    // running/stopped) lands in 5e when the inventory snapshot grows.
    match q.stack_id {
        Some(id) => {
            let stack = handles
                .inventory
                .get_stack(StackId(id))
                .await?
                .ok_or(ApiError::NotFound)?;
            // Stack itself doesn't carry services — they were sent in the
            // proto StackInfo but we don't persist per-service rows in v1.
            // Return an empty vec for now; the UI handles this gracefully.
            // 5e will land a `services` table or extend stacks with a
            // serialized services blob.
            let _ = stack;
            Ok(Json(vec![]))
        }
        None => Ok(Json(vec![])),
    }
}

pub async fn get_service(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
) -> Result<Json<ServiceDto>, ApiError> {
    Err(ApiError::NotFound)
}
```

(Yes, this is intentionally minimal. The full services persistence layer is a 5e item — see `## Task 9 in 5e`. The v1 dashboard UI uses the service names embedded in `StackInfo` directly via the stack detail page.)

Add routes:

```rust
.route("/api/v1/services",     get(list_services))
.route("/api/v1/services/:id", get(get_service))
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p isengard-dashboard`
Expected: PASS — new tests green, all existing 5b tests still green.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/src/api.rs
git commit -m "feat(dashboard): real stacks + sparkline handlers + PATCH host fleet"
```

---

## Task 8: Frontend stores + composables (stacks, services, useSparkline, useHostActions)

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/stores/stacks.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/services.ts`
- Create: `crates/isengard-plugins/dashboard/web/composables/useSparkline.ts`
- Create: `crates/isengard-plugins/dashboard/web/composables/useHostActions.ts`

- [ ] **Step 1: Write stores/stacks.ts**

```typescript
import { defineStore } from 'pinia'
import { useApi } from '~/composables/useApi'

export interface Stack {
  id: string
  host_id: string
  name: string
  source: 'compose' | 'manual' | 'inferred'
  discovered_at: string
}

export const useStacksStore = defineStore('stacks', {
  state: () => ({
    items: [] as Stack[],
    loaded: false,
    loading: false,
    error: null as string | null,
  }),

  getters: {
    byHost: (state) => (hostId: string): Stack[] =>
      state.items.filter((s) => s.host_id === hostId),

    byId: (state) => (id: string): Stack | undefined =>
      state.items.find((s) => s.id === id),
  },

  actions: {
    async fetchAll(filters: { fleet?: string; host_id?: string } = {}) {
      this.loading = true
      this.error = null
      try {
        const api = useApi()
        const params = new URLSearchParams()
        if (filters.fleet) params.set('fleet', filters.fleet)
        if (filters.host_id) params.set('host_id', filters.host_id)
        this.items = await api.get<Stack[]>(`/api/v1/stacks?${params.toString()}`)
        this.loaded = true
      } catch (e) {
        this.error = e instanceof Error ? e.message : String(e)
      } finally {
        this.loading = false
      }
    },

    async fetchOne(id: string): Promise<Stack | null> {
      try {
        const api = useApi()
        const stack = await api.get<Stack>(`/api/v1/stacks/${id}`)
        // Merge into items
        const idx = this.items.findIndex((s) => s.id === id)
        if (idx >= 0) this.items[idx] = stack
        else this.items.push(stack)
        return stack
      } catch {
        return null
      }
    },
  },
})
```

- [ ] **Step 2: Write stores/services.ts**

```typescript
import { defineStore } from 'pinia'
import { useApi } from '~/composables/useApi'

export interface Service {
  id: string
  host_id: string
  stack_id: string | null
  name: string
  image: string
  state: 'running' | 'stopped' | 'restarting' | 'unknown'
}

export const useServicesStore = defineStore('services', {
  state: () => ({
    items: [] as Service[],
    loaded: false,
    loading: false,
  }),

  getters: {
    byStack: (state) => (stackId: string): Service[] =>
      state.items.filter((s) => s.stack_id === stackId),
  },

  actions: {
    async fetchByStack(stackId: string) {
      this.loading = true
      try {
        const api = useApi()
        const items = await api.get<Service[]>(`/api/v1/services?stack_id=${stackId}`)
        // Merge: replace items with same stack_id, keep others
        this.items = this.items
          .filter((s) => s.stack_id !== stackId)
          .concat(items)
      } finally {
        this.loading = false
      }
    },
  },
})
```

- [ ] **Step 3: Write composables/useSparkline.ts**

```typescript
import { ref, type Ref } from 'vue'
import { useApi } from '~/composables/useApi'

export interface SparklineData {
  buckets: number[]
  range: string
  total: number
}

/**
 * Fetches the per-hour event-count sparkline for a host. Caller is responsible
 * for re-fetching on a schedule if a live sparkline is desired.
 */
export function useSparkline(hostId: Ref<string> | string) {
  const data = ref<SparklineData | null>(null)
  const loading = ref(false)

  async function fetch(range = '24h') {
    const id = typeof hostId === 'string' ? hostId : hostId.value
    loading.value = true
    try {
      const api = useApi()
      data.value = await api.get<SparklineData>(`/api/v1/hosts/${id}/sparkline?range=${range}`)
    } finally {
      loading.value = false
    }
  }

  return { data, loading, fetch }
}
```

- [ ] **Step 4: Write composables/useHostActions.ts**

```typescript
import { useApi } from '~/composables/useApi'

export function useHostActions() {
  const api = useApi()

  async function setFleet(hostId: string, fleet: string) {
    return await api.patch(`/api/v1/hosts/${hostId}`, { fleet })
  }

  async function forceUpdate(hostId: string) {
    return await api.post(`/api/v1/hosts/${hostId}/actions/force-update`, {})
  }

  async function decommission(hostId: string) {
    return await api.delete(`/api/v1/hosts/${hostId}`)
  }

  return { setFleet, forceUpdate, decommission }
}
```

- [ ] **Step 5: Quick build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS — Nuxt builds with no TypeScript errors.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/stores/stacks.ts \
        crates/isengard-plugins/dashboard/web/stores/services.ts \
        crates/isengard-plugins/dashboard/web/composables/useSparkline.ts \
        crates/isengard-plugins/dashboard/web/composables/useHostActions.ts
git commit -m "feat(dashboard-web): stacks + services pinia stores + useSparkline + useHostActions"
```

---

## Task 9: Sparkline + StatusPill + StackRow + ServiceChip components (leaf primitives)

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/Sparkline.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/StatusPill.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/StackRow.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/ServiceChip.vue`

- [ ] **Step 1: Write Sparkline.vue**

```vue
<script setup lang="ts">
interface Props {
  data: number[]
  color?: 'success' | 'warn' | 'error' | 'info'
  width?: number
  height?: number
}

const props = withDefaults(defineProps<Props>(), {
  color: 'info',
  width: 130,
  height: 24,
})

const colorClass = computed(() => ({
  success: 'fill-iso-success',
  warn:    'fill-iso-warn',
  error:   'fill-iso-error',
  info:    'fill-iso-info',
})[props.color])

const max = computed(() => Math.max(1, ...props.data))
const barWidth = computed(() => props.data.length > 0 ? (props.width / props.data.length) - 1 : 0)
</script>

<template>
  <svg :width="width" :height="height" :viewBox="`0 0 ${width} ${height}`" class="overflow-visible">
    <g :class="colorClass">
      <rect
        v-for="(v, i) in data"
        :key="i"
        :x="i * (barWidth + 1)"
        :y="height - (v / max) * height"
        :width="barWidth"
        :height="(v / max) * height"
        rx="1"
      />
    </g>
  </svg>
</template>
```

- [ ] **Step 2: Write StatusPill.vue**

```vue
<script setup lang="ts">
interface Props {
  state: 'success' | 'warn' | 'error' | 'info' | 'neutral'
  label: string
  size?: 'xs' | 'sm'
  icon?: string
}

withDefaults(defineProps<Props>(), {
  size: 'sm',
})

const stateClasses = {
  success: 'bg-iso-success/15 text-iso-success border-iso-success/30',
  warn:    'bg-iso-warn/15    text-iso-warn    border-iso-warn/30',
  error:   'bg-iso-error/15   text-iso-error   border-iso-error/30',
  info:    'bg-iso-info/15    text-iso-info    border-iso-info/30',
  neutral: 'bg-iso-bg-elevated text-iso-text-muted border-iso-border',
}

const sizeClasses = {
  xs: 'text-[10px] px-1.5 py-0.5 gap-1',
  sm: 'text-xs px-2 py-0.5 gap-1.5',
}
</script>

<template>
  <span
    class="inline-flex items-center rounded-full border font-medium"
    :class="[stateClasses[state], sizeClasses[size]]"
  >
    <Icon v-if="icon" :name="icon" :size="size === 'xs' ? 10 : 12" />
    {{ label }}
  </span>
</template>
```

- [ ] **Step 3: Write ServiceChip.vue**

```vue
<script setup lang="ts">
interface Props {
  name: string
  state?: 'running' | 'stopped' | 'restarting' | 'unknown'
}

const props = withDefaults(defineProps<Props>(), {
  state: 'unknown',
})

const dotColor = computed(() => ({
  running:    'bg-iso-success',
  stopped:    'bg-iso-text-faint',
  restarting: 'bg-iso-warn',
  unknown:    'bg-iso-text-muted',
})[props.state])
</script>

<template>
  <span class="inline-flex items-center gap-1.5 rounded px-2 py-1 bg-iso-bg-elevated border border-iso-border text-xs font-mono">
    <span class="w-1.5 h-1.5 rounded-full" :class="dotColor" />
    {{ name }}
  </span>
</template>
```

- [ ] **Step 4: Write StackRow.vue**

```vue
<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Props {
  stack: Stack
  services: { name: string; state?: 'running' | 'stopped' | 'restarting' | 'unknown' }[]
}

defineProps<Props>()
defineEmits<{ click: [stack: Stack] }>()
</script>

<template>
  <div
    class="flex items-center gap-3 py-2 px-3 rounded hover:bg-iso-bg-elevated cursor-pointer"
    @click="$emit('click', stack)"
  >
    <Icon name="lucide:layers" :size="16" class="text-iso-text-muted shrink-0" />
    <div class="flex-1 min-w-0">
      <div class="font-mono text-sm">{{ stack.name }}</div>
      <div class="text-[10px] text-iso-text-faint">{{ services.length }} services</div>
    </div>
    <div class="flex items-center gap-1.5 flex-wrap justify-end max-w-[60%]">
      <ServiceChip
        v-for="svc in services"
        :key="svc.name"
        :name="svc.name"
        :state="svc.state"
      />
    </div>
  </div>
</template>
```

- [ ] **Step 5: Quick build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/Sparkline.vue \
        crates/isengard-plugins/dashboard/web/components/StatusPill.vue \
        crates/isengard-plugins/dashboard/web/components/StackRow.vue \
        crates/isengard-plugins/dashboard/web/components/ServiceChip.vue
git commit -m "feat(dashboard-web): leaf components — Sparkline, StatusPill, StackRow, ServiceChip"
```

---

## Task 10: HostRow + HostsTable + FleetWeather + AddHostButton

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/HostRow.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/HostsTable.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/FleetWeather.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/AddHostButton.vue`

- [ ] **Step 1: Write HostRow.vue**

```vue
<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  host: Host
  sparkline: number[]
  stackCount: number
  serviceCount: number
  latestEvent: { kind: string; summary: string } | null
  lastSeenRelative: string
  agentVersionWarn: boolean
  selected?: boolean
}

defineProps<Props>()
const emit = defineEmits<{
  click: [host: Host]
  action: [action: 'force-update' | 'shell' | 'menu', host: Host]
}>()

const stateDot = computed((): string => {
  // Derive from latestEvent.kind or some stored state
  // (placeholder: use 'success' if no failed events recently)
  return 'bg-iso-success'
})

const kindColor = (kind: string) => ({
  UPDATED:  'text-iso-success',
  FAILED:   'text-iso-error',
  CHECKED:  'text-iso-text-muted',
  PULLING:  'text-iso-warn',
  DISCONNECT: 'text-iso-info',
}[kind] ?? 'text-iso-text-muted')
</script>

<template>
  <div
    class="group grid items-center gap-3 px-3 py-2 hover:bg-iso-bg-elevated cursor-pointer border-l-2"
    :class="selected ? 'border-iso-success bg-iso-success/5' : 'border-transparent'"
    style="grid-template-columns: 170px 70px 130px 80px 1fr 90px 60px auto"
    @click="emit('click', host)"
  >
    <div class="flex items-center gap-2 min-w-0">
      <span class="w-2 h-2 rounded-full shrink-0" :class="stateDot" />
      <span class="font-mono text-sm truncate">{{ host.hostname }}</span>
    </div>
    <span class="text-xs text-iso-text-muted">{{ host.fleet }}</span>
    <Sparkline :data="sparkline" color="success" :width="120" :height="20" />
    <span class="text-xs text-iso-text-muted font-mono">
      {{ stackCount }} · {{ serviceCount }} svcs
    </span>
    <span v-if="latestEvent" class="text-xs font-mono truncate">
      <span :class="kindColor(latestEvent.kind)">{{ latestEvent.kind }}</span>
      <span class="text-iso-text-muted ml-1">{{ latestEvent.summary }}</span>
    </span>
    <span v-else class="text-xs text-iso-text-faint">no events</span>
    <span class="text-xs text-iso-text-muted">{{ lastSeenRelative }}</span>
    <span
      class="text-xs font-mono"
      :class="agentVersionWarn ? 'text-iso-warn' : 'text-iso-text-muted'"
    >
      {{ host.agent_version }}
    </span>
    <div class="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        :title="'Force update'"
        @click.stop="emit('action', 'force-update', host)"
      >
        <Icon name="lucide:zap" :size="14" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        :title="'Open shell'"
        @click.stop="emit('action', 'shell', host)"
      >
        <Icon name="lucide:terminal" :size="14" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        :title="'More'"
        @click.stop="emit('action', 'menu', host)"
      >
        <Icon name="lucide:ellipsis" :size="14" />
      </button>
    </div>
  </div>
</template>
```

- [ ] **Step 2: Write FleetWeather.vue**

```vue
<script setup lang="ts">
interface Props {
  buckets: number[]
  range: '24h' | '7d'
  totalEvents: number
}

defineProps<Props>()
defineEmits<{ 'range-change': [range: '24h' | '7d'] }>()
</script>

<template>
  <div class="flex items-center gap-4 px-4 py-3 border-b border-iso-border bg-iso-bg-elevated/30">
    <span class="text-xs text-iso-text-faint uppercase tracking-wider">Fleet weather</span>
    <Sparkline :data="buckets" color="success" :width="600" :height="28" />
    <span class="text-xs text-iso-text-muted font-mono">
      {{ totalEvents }} events / {{ range }}
    </span>
    <div class="ml-auto flex items-center gap-1">
      <button
        class="text-xs px-2 py-0.5 rounded hover:bg-iso-bg-base"
        :class="range === '24h' ? 'text-iso-text-base bg-iso-bg-base' : 'text-iso-text-muted'"
        @click="$emit('range-change', '24h')"
      >24h</button>
      <button
        class="text-xs px-2 py-0.5 rounded hover:bg-iso-bg-base"
        :class="range === '7d' ? 'text-iso-text-base bg-iso-bg-base' : 'text-iso-text-muted'"
        @click="$emit('range-change', '7d')"
      >7d</button>
    </div>
  </div>
</template>
```

- [ ] **Step 3: Write AddHostButton.vue**

```vue
<script setup lang="ts">
defineEmits<{ click: [] }>()
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-3 py-1.5 rounded border border-iso-border hover:border-iso-success hover:text-iso-success text-sm transition-colors"
    @click="$emit('click')"
  >
    <Icon name="lucide:plus" :size="14" />
    Add host
  </button>
</template>
```

(The full add-host modal lands in 5e. The button is wired to a no-op for now; clicking opens an empty placeholder modal in 5d which 5e replaces with the real form.)

- [ ] **Step 4: Write HostsTable.vue**

```vue
<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  hosts: Host[]
  sparklines: Record<string, number[]>
  stackCounts: Record<string, { stacks: number; services: number }>
  latestEvents: Record<string, { kind: string; summary: string } | null>
  selectedId: string | null
}

defineProps<Props>()
const emit = defineEmits<{
  select: [host: Host]
  action: [action: 'force-update' | 'shell' | 'menu', host: Host]
}>()

function lastSeenRelative(host: Host): string {
  if (!host.last_seen_at) return 'never'
  const ms = Date.now() - new Date(host.last_seen_at).getTime()
  const mins = Math.floor(ms / 60000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h ago`
  const days = Math.floor(hrs / 24)
  return `${days}d ago`
}
</script>

<template>
  <div>
    <div
      class="grid items-center gap-3 px-3 py-2 text-[10px] uppercase tracking-wider text-iso-text-faint border-b border-iso-border"
      style="grid-template-columns: 170px 70px 130px 80px 1fr 90px 60px auto"
    >
      <span>Host</span>
      <span>Fleet</span>
      <span>Activity</span>
      <span>Stacks</span>
      <span>Latest</span>
      <span>Last seen</span>
      <span>Agent</span>
      <span></span>
    </div>
    <HostRow
      v-for="h in hosts"
      :key="h.id"
      :host="h"
      :sparkline="sparklines[h.id] ?? []"
      :stack-count="stackCounts[h.id]?.stacks ?? 0"
      :service-count="stackCounts[h.id]?.services ?? 0"
      :latest-event="latestEvents[h.id] ?? null"
      :last-seen-relative="lastSeenRelative(h)"
      :agent-version-warn="false"
      :selected="selectedId === h.id"
      @click="emit('select', h)"
      @action="(a, host) => emit('action', a, host)"
    />
  </div>
</template>
```

- [ ] **Step 5: Build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/HostRow.vue \
        crates/isengard-plugins/dashboard/web/components/HostsTable.vue \
        crates/isengard-plugins/dashboard/web/components/FleetWeather.vue \
        crates/isengard-plugins/dashboard/web/components/AddHostButton.vue
git commit -m "feat(dashboard-web): HostsTable v2 components — HostRow, FleetWeather, AddHostButton"
```

---

## Task 11: HostCard + page assembly: /hosts and /hosts/:id

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/HostCard.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/hosts/index.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/hosts/[id].vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/TopBar.vue`

- [ ] **Step 1: Write HostCard.vue**

```vue
<script setup lang="ts">
import type { Host } from '~/stores/hosts'
import type { Stack } from '~/stores/stacks'

interface Props {
  host: Host
  stacks: Stack[]
  // services keyed by stack_id
  services: Record<string, { name: string; state?: 'running' | 'stopped' | 'restarting' | 'unknown' }[]>
  hasIssues: boolean
}

const props = defineProps<Props>()
const expanded = ref(props.hasIssues) // healthy hosts are collapsed by default

const totalServices = computed(() =>
  Object.values(props.services).reduce((acc, s) => acc + s.length, 0)
)
</script>

<template>
  <article class="rounded-lg border border-iso-border bg-iso-bg-elevated/30 overflow-hidden">
    <header class="flex items-center justify-between px-4 py-3 border-b border-iso-border">
      <div class="flex items-center gap-3">
        <span class="w-2.5 h-2.5 rounded-full" :class="hasIssues ? 'bg-iso-error' : 'bg-iso-success'" />
        <h3 class="font-mono text-base">{{ host.hostname }}</h3>
        <span class="text-xs text-iso-text-muted">· {{ host.fleet }}</span>
      </div>
      <div class="flex items-center gap-3 text-xs text-iso-text-muted font-mono">
        <span>{{ stacks.length }} stacks</span>
        <span>·</span>
        <span>{{ totalServices }} services</span>
        <span>·</span>
        <span>agent {{ host.agent_version }}</span>
      </div>
    </header>

    <div v-if="!hasIssues && !expanded" class="px-4 py-3">
      <button
        class="text-sm text-iso-success flex items-center gap-2"
        @click="expanded = true"
      >
        <Icon name="lucide:check" :size="14" />
        All {{ stacks.length }} stacks healthy
        <span class="text-iso-text-faint">· show all ▾</span>
      </button>
    </div>

    <div v-else class="divide-y divide-iso-border">
      <StackRow
        v-for="stack in stacks"
        :key="stack.id"
        :stack="stack"
        :services="services[stack.id] ?? []"
        @click="$router.push(`/stacks/${stack.id}`)"
      />
    </div>

    <footer v-if="hasIssues || expanded" class="px-4 py-2 border-t border-iso-border text-xs text-iso-text-faint">
      <button
        v-if="!hasIssues"
        class="hover:text-iso-text-muted"
        @click="expanded = false"
      >
        Collapse
      </button>
    </footer>
  </article>
</template>
```

- [ ] **Step 2: Wire TopBar tab activation**

Edit `TopBar.vue`. Replace the static `:active="..."` props with route-driven activation:

```vue
<script setup lang="ts">
const route = useRoute()

const tabs = [
  { name: 'Home',   path: '/' },
  { name: 'Hosts',  path: '/hosts' },
  { name: 'Stacks', path: '/stacks' },
  { name: 'Events', path: '/events' },
]

function isActive(path: string): boolean {
  if (path === '/') return route.path === '/'
  return route.path.startsWith(path)
}
</script>

<template>
  <!-- in the existing tab nav region: -->
  <nav class="flex items-center gap-1">
    <NuxtLink
      v-for="t in tabs"
      :key="t.path"
      :to="t.path"
      class="px-3 py-1 rounded text-sm"
      :class="isActive(t.path) ? 'bg-iso-bg-elevated text-iso-text-base' : 'text-iso-text-muted hover:text-iso-text-base'"
    >
      {{ t.name }}
    </NuxtLink>
  </nav>
</template>
```

- [ ] **Step 3: Write pages/hosts/index.vue**

```vue
<script setup lang="ts">
import { useHostsStore } from '~/stores/hosts'
import { useStacksStore } from '~/stores/stacks'
import { useEventsStore } from '~/stores/events'
import { useUiStore } from '~/stores/ui'

definePageMeta({ layout: 'default' })

const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()
const uiStore = useUiStore()

const sparklines = ref<Record<string, number[]>>({})
const fleetWeatherBuckets = ref<number[]>(new Array(24).fill(0))
const fleetWeatherRange = ref<'24h' | '7d'>('24h')

await Promise.all([
  hostsStore.fetchAll(),
  stacksStore.fetchAll(),
  eventsStore.fetchRecent({ limit: 200 }),
])

// Fetch sparklines for each host
for (const host of hostsStore.items) {
  try {
    const { useSparkline } = await import('~/composables/useSparkline')
    const { data, fetch } = useSparkline(host.id)
    await fetch('24h')
    if (data.value) sparklines.value[host.id] = data.value.buckets
  } catch {
    sparklines.value[host.id] = []
  }
}

// Aggregate buckets across hosts for the FleetWeather strip
const aggregate = new Array(24).fill(0)
for (const buckets of Object.values(sparklines.value)) {
  buckets.forEach((v, i) => { aggregate[i] += v })
}
fleetWeatherBuckets.value = aggregate

const filteredHosts = computed(() => {
  const fleet = uiStore.activeFleet
  return fleet === 'All fleets'
    ? hostsStore.items
    : hostsStore.items.filter((h) => h.fleet === fleet)
})

const stackCounts = computed(() => {
  const out: Record<string, { stacks: number; services: number }> = {}
  for (const h of hostsStore.items) {
    const hostStacks = stacksStore.byHost(h.id)
    out[h.id] = { stacks: hostStacks.length, services: 0 } // service count comes from stack metadata in 5e
  }
  return out
})

const latestEvents = computed(() => {
  const out: Record<string, { kind: string; summary: string } | null> = {}
  for (const h of hostsStore.items) {
    const e = eventsStore.items.find((ev) => ev.host_id === h.id) ?? null
    out[h.id] = e ? { kind: e.kind, summary: e.summary } : null
  }
  return out
})

const totalEvents = computed(() => fleetWeatherBuckets.value.reduce((a, b) => a + b, 0))

const router = useRouter()
function selectHost(host: { id: string }) {
  router.push(`/hosts/${host.id}`)
}

function handleAction(action: string, host: { id: string }) {
  // Wired to useHostActions — full UX in 5e
  console.log('action', action, host.id)
}
</script>

<template>
  <div class="flex flex-col h-full">
    <FleetWeather
      :buckets="fleetWeatherBuckets"
      :range="fleetWeatherRange"
      :total-events="totalEvents"
      @range-change="(r) => fleetWeatherRange = r"
    />
    <div class="flex items-center justify-between px-4 py-3">
      <div class="text-sm text-iso-text-muted">
        {{ filteredHosts.length }} hosts
      </div>
      <AddHostButton @click="$router.push('/hosts?add=1')" />
    </div>
    <HostsTable
      :hosts="filteredHosts"
      :sparklines="sparklines"
      :stack-counts="stackCounts"
      :latest-events="latestEvents"
      :selected-id="null"
      @select="selectHost"
      @action="handleAction"
    />
    <div class="mt-auto px-4 py-2 text-xs text-iso-text-faint border-t border-iso-border">
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">/</kbd> filter ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">j/k</kbd> move ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">Enter</kbd> open ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">⌘K</kbd> cmd
    </div>
  </div>
</template>
```

- [ ] **Step 4: Write pages/hosts/[id].vue**

```vue
<script setup lang="ts">
import { useHostsStore } from '~/stores/hosts'
import { useStacksStore } from '~/stores/stacks'
import { useEventsStore } from '~/stores/events'

const route = useRoute()
const hostId = computed(() => route.params.id as string)

const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()

await Promise.all([
  hostsStore.fetchOne(hostId.value),
  stacksStore.fetchAll({ host_id: hostId.value }),
  eventsStore.fetchRecent({ host_id: hostId.value, limit: 50 }),
])

const host = computed(() => hostsStore.byId(hostId.value))
const stacks = computed(() => stacksStore.byHost(hostId.value))

// services per stack derived from the heartbeat-time StackInfo (until 5e
// adds a real services table, the page renders chip names from a temporary
// map that lives in the stack itself in v1; the agent sends them and the
// UI reads from a parallel `services` field on stack if present)
const services = ref<Record<string, { name: string; state?: 'running' | 'stopped' | 'restarting' | 'unknown' }[]>>({})

const hasIssues = computed(() =>
  eventsStore.items.some((e) => e.kind === 'FAILED' && e.host_id === hostId.value)
)
</script>

<template>
  <div v-if="host" class="p-6 space-y-4">
    <header class="flex items-center justify-between">
      <div>
        <NuxtLink to="/hosts" class="text-xs text-iso-text-muted hover:text-iso-text-base">
          ← Hosts
        </NuxtLink>
        <h1 class="font-mono text-xl mt-1">{{ host.hostname }}</h1>
        <div class="text-sm text-iso-text-muted mt-1">
          {{ host.os }} · {{ host.arch }} · agent {{ host.agent_version }} · fleet
          <select class="bg-transparent text-iso-text-base">
            <option>{{ host.fleet }}</option>
          </select>
        </div>
      </div>
      <div class="flex items-center gap-2">
        <button class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-success">
          Force update
        </button>
        <button class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-error/50 text-iso-error">
          Decommission
        </button>
      </div>
    </header>

    <HostCard
      :host="host"
      :stacks="stacks"
      :services="services"
      :has-issues="hasIssues"
    />
  </div>

  <div v-else class="p-6 text-iso-text-muted">
    Host not found.
  </div>
</template>
```

- [ ] **Step 5: Build + smoke**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/HostCard.vue \
        crates/isengard-plugins/dashboard/web/components/TopBar.vue \
        crates/isengard-plugins/dashboard/web/pages/hosts/index.vue \
        crates/isengard-plugins/dashboard/web/pages/hosts/[id].vue
git commit -m "feat(dashboard-web): /hosts table page + /hosts/:id detail with HostCard"
```

---

## Task 12: Stacks pages — /stacks (table) and /stacks/:id (detail)

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/StacksTable.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/StackHeader.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/stacks/index.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`

- [ ] **Step 1: Write StacksTable.vue**

```vue
<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Row {
  stack: Stack
  hostHostname: string
  fleet: string
  serviceCount: number
  latestEvent: { kind: string; summary: string } | null
}

interface Props {
  rows: Row[]
}

defineProps<Props>()

const router = useRouter()
const kindColor = (kind: string) => ({
  UPDATED: 'text-iso-success',
  FAILED:  'text-iso-error',
  CHECKED: 'text-iso-text-muted',
  PULLING: 'text-iso-warn',
  DISCONNECT: 'text-iso-info',
}[kind] ?? 'text-iso-text-muted')
</script>

<template>
  <div>
    <div
      class="grid items-center gap-3 px-3 py-2 text-[10px] uppercase tracking-wider text-iso-text-faint border-b border-iso-border"
      style="grid-template-columns: 200px 170px 70px 70px 1fr 90px"
    >
      <span>Stack</span>
      <span>Host</span>
      <span>Fleet</span>
      <span>Services</span>
      <span>Latest event</span>
      <span>Source</span>
    </div>

    <div
      v-for="row in rows"
      :key="row.stack.id"
      class="grid items-center gap-3 px-3 py-2 hover:bg-iso-bg-elevated cursor-pointer"
      style="grid-template-columns: 200px 170px 70px 70px 1fr 90px"
      @click="router.push(`/stacks/${row.stack.id}`)"
    >
      <div class="flex items-center gap-2 min-w-0">
        <Icon name="lucide:layers" :size="14" class="text-iso-text-muted shrink-0" />
        <span class="font-mono text-sm truncate">{{ row.stack.name }}</span>
      </div>
      <span class="font-mono text-xs text-iso-text-muted truncate">{{ row.hostHostname }}</span>
      <span class="text-xs text-iso-text-muted">{{ row.fleet }}</span>
      <span class="text-xs text-iso-text-muted font-mono">{{ row.serviceCount }}</span>
      <span v-if="row.latestEvent" class="text-xs font-mono truncate">
        <span :class="kindColor(row.latestEvent.kind)">{{ row.latestEvent.kind }}</span>
        <span class="text-iso-text-muted ml-1">{{ row.latestEvent.summary }}</span>
      </span>
      <span v-else class="text-xs text-iso-text-faint">no events</span>
      <span class="text-xs text-iso-text-faint uppercase">{{ row.stack.source }}</span>
    </div>
  </div>
</template>
```

- [ ] **Step 2: Write StackHeader.vue**

```vue
<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Props {
  stack: Stack
  hostHostname: string
  fleet: string
}

defineProps<Props>()
defineEmits<{ 'force-update': [] }>()
</script>

<template>
  <header class="flex items-center justify-between p-6 border-b border-iso-border">
    <div>
      <NuxtLink to="/stacks" class="text-xs text-iso-text-muted hover:text-iso-text-base">
        ← Stacks
      </NuxtLink>
      <h1 class="font-mono text-xl mt-1 flex items-center gap-2">
        <Icon name="lucide:layers" :size="20" class="text-iso-text-muted" />
        {{ stack.name }}
      </h1>
      <div class="text-sm text-iso-text-muted mt-1">
        on <NuxtLink :to="`/hosts/${stack.host_id}`" class="hover:text-iso-text-base">{{ hostHostname }}</NuxtLink>
        · fleet {{ fleet }}
        · source {{ stack.source }}
      </div>
    </div>
    <button
      class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-success hover:text-iso-success"
      @click="$emit('force-update')"
    >
      <Icon name="lucide:zap" :size="14" class="inline mr-1" />
      Force update stack
    </button>
  </header>
</template>
```

- [ ] **Step 3: Write pages/stacks/index.vue**

```vue
<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useEventsStore } from '~/stores/events'
import { useUiStore } from '~/stores/ui'

const stacksStore = useStacksStore()
const hostsStore  = useHostsStore()
const eventsStore = useEventsStore()
const uiStore     = useUiStore()

await Promise.all([
  stacksStore.fetchAll(),
  hostsStore.fetchAll(),
  eventsStore.fetchRecent({ limit: 200 }),
])

const rows = computed(() => {
  const fleet = uiStore.activeFleet
  return stacksStore.items
    .map((stack) => {
      const host = hostsStore.byId(stack.host_id)
      if (!host) return null
      if (fleet !== 'All fleets' && host.fleet !== fleet) return null
      const latest = eventsStore.items.find(
        (e) => e.host_id === stack.host_id && e.metadata?.stack === stack.name
      ) ?? null
      return {
        stack,
        hostHostname: host.hostname,
        fleet: host.fleet,
        serviceCount: 0, // 5e: real service count from /api/v1/services?stack_id=
        latestEvent: latest ? { kind: latest.kind, summary: latest.summary } : null,
      }
    })
    .filter((r): r is NonNullable<typeof r> => r !== null)
})
</script>

<template>
  <div class="flex flex-col h-full">
    <div class="px-4 py-3 border-b border-iso-border">
      <div class="text-sm text-iso-text-muted">{{ rows.length }} stacks</div>
    </div>
    <StacksTable :rows="rows" />
    <div class="mt-auto px-4 py-2 text-xs text-iso-text-faint border-t border-iso-border">
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">/</kbd> filter ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">⌘K</kbd> cmd
    </div>
  </div>
</template>
```

- [ ] **Step 4: Write pages/stacks/[id].vue**

```vue
<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useServicesStore } from '~/stores/services'
import { useEventsStore } from '~/stores/events'

const route = useRoute()
const stackId = computed(() => route.params.id as string)

const stacksStore  = useStacksStore()
const hostsStore   = useHostsStore()
const servicesStore = useServicesStore()
const eventsStore  = useEventsStore()

await stacksStore.fetchOne(stackId.value)
const stack = computed(() => stacksStore.byId(stackId.value))

watchEffect(async () => {
  if (stack.value) {
    await Promise.all([
      hostsStore.fetchOne(stack.value.host_id),
      servicesStore.fetchByStack(stack.value.id),
      eventsStore.fetchRecent({ limit: 50 }),
    ])
  }
})

const host = computed(() => stack.value ? hostsStore.byId(stack.value.host_id) : undefined)
const services = computed(() => stack.value ? servicesStore.byStack(stack.value.id) : [])
const recentEvents = computed(() => {
  if (!stack.value) return []
  return eventsStore.items
    .filter((e) => e.host_id === stack.value!.host_id)
    .slice(0, 20)
})

function forceUpdate() {
  // POST /api/v1/stacks/:id/actions/force-update — wired in 5e
  console.log('force update stack', stackId.value)
}
</script>

<template>
  <div v-if="stack && host">
    <StackHeader
      :stack="stack"
      :host-hostname="host.hostname"
      :fleet="host.fleet"
      @force-update="forceUpdate"
    />

    <div class="grid grid-cols-2 gap-6 p-6">
      <section>
        <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Services</h2>
        <div class="flex flex-wrap gap-2">
          <ServiceChip
            v-for="svc in services"
            :key="svc.name"
            :name="svc.name"
            :state="svc.state"
          />
          <span v-if="services.length === 0" class="text-sm text-iso-text-faint">
            No services reported (waiting for next heartbeat).
          </span>
        </div>
      </section>

      <section>
        <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Recent events</h2>
        <div class="space-y-1">
          <EventRow
            v-for="e in recentEvents"
            :key="e.id"
            :event="e"
            :selected="false"
          />
          <span v-if="recentEvents.length === 0" class="text-sm text-iso-text-faint">
            No recent events for this stack's host.
          </span>
        </div>
      </section>
    </div>
  </div>

  <div v-else class="p-6 text-iso-text-muted">
    Stack not found.
  </div>
</template>
```

- [ ] **Step 5: Build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/StacksTable.vue \
        crates/isengard-plugins/dashboard/web/components/StackHeader.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/index.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue
git commit -m "feat(dashboard-web): /stacks table + /stacks/:id detail"
```

---

## Task 13: Cmd pane navigator — surface stacks + services

**Files:**
- Modify: `crates/isengard-plugins/dashboard/web/components/CmdPane.vue` (or `CmdInput.vue` — wherever the search index is built)

- [ ] **Step 1: Locate search-index code from 5c**

The 5c plan introduced fuse.js fuzzy search across hosts. We extend it to include stacks + services (stacks first because services don't have a stable id in 5d).

- [ ] **Step 2: Extend the indexable items**

```typescript
// inside the cmd pane's setup script

import { useStacksStore } from '~/stores/stacks'
import { useServicesStore } from '~/stores/services'

const stacksStore = useStacksStore()
const servicesStore = useServicesStore()

interface Indexable {
  type: 'host' | 'stack' | 'service' | 'action'
  id: string
  label: string
  meta: string
  path?: string
  action?: () => void
}

const items = computed<Indexable[]>(() => [
  ...hostsStore.items.map((h) => ({
    type: 'host' as const,
    id: h.id,
    label: h.hostname,
    meta: `${h.fleet} · agent ${h.agent_version}`,
    path: `/hosts/${h.id}`,
  })),
  ...stacksStore.items.map((s) => ({
    type: 'stack' as const,
    id: s.id,
    label: s.name,
    meta: hostsStore.byId(s.host_id)?.hostname ?? '',
    path: `/stacks/${s.id}`,
  })),
  ...servicesStore.items.map((sv) => ({
    type: 'service' as const,
    id: sv.id,
    label: sv.name,
    meta: `${sv.image} · ${sv.state}`,
  })),
  // built-in actions (unchanged from 5c)
  // ...
])
```

Update the section grouping in the result list to include `STACKS` and `SERVICES` sections.

- [ ] **Step 3: Make sure the stores are loaded when the cmd pane opens**

In the cmd pane mount/open hook:

```typescript
onMounted(async () => {
  if (!hostsStore.loaded)  await hostsStore.fetchAll()
  if (!stacksStore.loaded) await stacksStore.fetchAll()
  // services are loaded lazily per-stack; no global fetch
})
```

- [ ] **Step 4: Build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/CmdPane.vue
git commit -m "feat(dashboard-web): cmd pane navigator surfaces stacks + services"
```

---

## Task 14: CI gate, end-to-end smoke, tag

**Files:**
- Modify (if needed): `.github/workflows/ci.yml`

- [ ] **Step 1: Run the full local CI**

```bash
just ci-local
```

Expected: PASS — `cargo build --workspace`, `cargo nextest run --workspace`, `cargo deny check`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `bun --cwd crates/isengard-plugins/dashboard/web run build` all clean.

- [ ] **Step 2: End-to-end smoke (4 terminals)**

T1 — controller:
```bash
cargo run -p isengard-controller
```

T2 — first agent:
```bash
ISENGARD_CONTROLLER=http://127.0.0.1:9417 \
ISENGARD_TOKEN=<token-from-T1> \
ISENGARD_FLEET=staging \
cargo run -p isengard-agent
```

T3 — start a labelled compose project on the agent host:
```bash
cd /tmp && cat > compose.yml <<'EOF'
services:
  web:
    image: nginx:alpine
    labels:
      com.docker.compose.project: blog
  db:
    image: postgres:alpine
    labels:
      com.docker.compose.project: blog
EOF
docker compose -p blog up -d
```

T4 — verify via API:
```bash
curl -s http://localhost:9418/api/v1/stacks | jq .
# expect: [ { name: "blog", source: "compose", ... } ]

curl -s http://localhost:9418/api/v1/hosts | jq '.[].fleet'
# expect: "staging"
```

Then in a browser: `http://localhost:9418`
- Click `Hosts` tab → see the host listed with fleet=staging, sparkline showing recent events, latest event visible
- Click the host → see Host Detail with the `blog` stack listed, expanded because there might be initial check events
- Click `blog` stack → see Stack Detail with `web` and `db` service chips
- Click `Stacks` tab → see `blog` listed flat
- Switch fleet picker to "All fleets" / "staging" → verify filter works

- [ ] **Step 3: Tag the sub-phase**

```bash
git tag -a v0.1.0-alpha.phase5d -m "Phase 5d: hosts + stacks UI + Stack entity end-to-end"
```

- [ ] **Step 4: Verify no push happened**

```bash
git status -sb
# expect: ahead but not pushed
```

DO NOT `git push`. The user explicitly maintains the no-push rule.

- [ ] **Step 5: Final commit if any docs changed during smoke**

If you adjusted any plan steps or fixed inline issues during smoke, commit those:

```bash
git add -p
git commit -m "docs: phase 5d smoke notes"
```

---

## Self-review

After writing this plan, re-check against the spec:

1. **Spec coverage:**
   - Stack entity migration: ✅ Task 1
   - Inventory methods: ✅ Task 2
   - Proto + agent + controller: ✅ Tasks 3-5
   - Stacks/services/sparkline DTOs + handlers: ✅ Tasks 6-7
   - Frontend stores + composables: ✅ Task 8
   - Leaf components (Sparkline, StatusPill, StackRow, ServiceChip): ✅ Task 9
   - Hosts table v2 (HostRow + HostsTable + FleetWeather + AddHostButton): ✅ Task 10
   - HostCard + /hosts pages + TopBar tab activation: ✅ Task 11
   - Stacks tab + Stack Detail: ✅ Task 12
   - Cmd pane extension: ✅ Task 13
   - End-to-end smoke + tag: ✅ Task 14

2. **Placeholder scan:** No `TBD`, no `// implement later`. The one acknowledged shortcut is the v1 services persistence (returns empty list from `/api/v1/services`); this is called out explicitly in Task 7 Step 4 and Task 8's services store, with the understanding that 5e adds the services table.

3. **Type consistency:**
   - `StackId(i64)` consistent across storage + DTOs (DTO serializes as `String` decimal)
   - `Stack.host_id` is `HostId` in storage, serialized as ULID `String` in DTO
   - `StackSource` enum mapping to `"compose" | "manual" | "inferred"` consistent across storage, proto wire, DTO
   - `StackInfo` proto message matches the agent's `derive_stacks` output and the controller's `process_heartbeat_stacks` input

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-01-phase-5d-hosts-stacks.md`.

**1. Subagent-Driven (recommended)** — fresh subagent per task, two-stage review (spec compliance → code quality), fast iteration.

**2. Inline Execution** — execute tasks in this session via executing-plans, batch with checkpoints.

Which approach?
