# Phase 2b: `isengard-storage` Crate + `Inventory` API

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a small `isengard-storage` crate that owns the SQLite layer for the controller. Phase 2b ships exactly the surface needed for enrollment and heartbeats: open the database, run migrations, enroll a host, mark it touched, look it up, list them all. Containers + journal arrive in later phases as new tables in new migrations.

**Architecture:** `isengard-storage` is its own workspace crate (so the dashboard plugin in Phase 5 can pull it in without dragging the entire controller). Public API: a `Host` value type, an `EnrollHost` request type, a `HostId` ULID newtype, and an `Inventory` that wraps a `sqlx::SqlitePool`. Migrations live alongside the source under `migrations/` and are bundled with `sqlx::migrate!()`. All operations are async.

**Tech Stack:** Rust 2024 edition, `sqlx` 0.8 (sqlite + runtime-tokio + macros + migrate features), `ulid` 1.x (ULID generation), `serde` for the public types, `tempfile` (dev-only) for tests.

**Branch:** `next` (currently green on GitHub after Phase 2a). Do NOT push without explicit approval.

**Spec:** `docs/superpowers/specs/2026-04-30-phase-2-enrollment-sync-design.md` §8 (Storage), §9 (Crate changes), §11 done state.

---

## Scope

**In:**
- New `isengard-storage` workspace crate
- First sqlx migration (`0001_hosts.sql`) creating the `hosts` table + `hosts_last_seen_at_idx`
- `HostId` (16-byte ULID newtype) with sqlx encode/decode + serde
- `Host` (the row type) and `EnrollHost` (the insert request type)
- `Inventory::open(path)` — opens SQLite, runs migrations
- `Inventory::enroll_host(req)` — inserts a row, returns the new `HostId`
- `Inventory::touch_host(id, ts)` — updates `last_seen_at`
- `Inventory::get_host(id)` — looks up one
- `Inventory::list_hosts()` — returns all
- Unit tests against a temp-dir SQLite file

**Out (not Phase 2b):**
- Wiring the controller to actually use the inventory (Phase 2c — Enroll handler)
- Containers table + journal table (Phase 3 + 4)
- Configuration push storage (Phase 3+)
- Async iteration / pagination (defer until fleet > 50 hosts)
- Tracing inside storage (add when Phase 2c needs it)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo test -p isengard-storage` runs ≥ 6 unit tests, all green
3. `cargo test --workspace` total ≥ 27 tests (21 from prior phases + ≥ 6 new)
4. `just ci-local` passes
5. CI on `origin/next` green
6. Tag `v0.1.0-alpha.phase2b` set locally
7. ~6–7 commits, each green on its own

---

## File Structure

```
isengard/
├── Cargo.toml                                             # MODIFY: + sqlx, ulid workspace deps; + isengard-storage path-dep
├── crates/
│   └── isengard-storage/
│       ├── Cargo.toml                                     # CREATE: deps + dev-deps
│       ├── migrations/
│       │   └── 0001_hosts.sql                             # CREATE: hosts table + index
│       └── src/
│           ├── lib.rs                                     # CREATE: re-exports + module decls
│           ├── error.rs                                   # CREATE: thiserror Error enum + Result alias
│           ├── host.rs                                    # CREATE: HostId, Host, EnrollHost
│           └── inventory.rs                               # CREATE: Inventory wrapper + CRUD methods + tests
```

---

## Task 1: Workspace deps + crate skeleton

**Files:**
- Modify: `Cargo.toml` (root)
- Create: `crates/isengard-storage/Cargo.toml`
- Create: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Add `sqlx` and `ulid` to root `[workspace.dependencies]`**

Open `~/Projects/isengard/Cargo.toml` and append after the `# gRPC + protobuf` block (which ends with `tokio-stream = "0.1.17"`), before the `# internal` block:

```toml
# storage
sqlx = { version = "0.8.2", default-features = false, features = ["runtime-tokio", "tls-rustls", "sqlite", "macros", "migrate", "chrono"] }
ulid = { version = "1.1.3", features = ["serde"] }
```

(`tls-rustls` is a transitive requirement of sqlx — without an explicit TLS feature, the crate fails to build. We're not using TLS for SQLite, but the feature still has to be selected.)

Then, in the `# internal` block, add a path-dep so other crates can pull `isengard-storage` via `.workspace = true`:

```toml
isengard-storage = { path = "crates/isengard-storage", version = "0.1.0-alpha" }
```

It should sit alongside `isengard-core`, `isengard-controller`, etc.

- [ ] **Step 2: Add the new crate to `[workspace] members`**

In the same `Cargo.toml`, the `members = [...]` array currently lists 8 crates. Add `"crates/isengard-storage",` to that list (insertion order doesn't matter; alphabetical is conventional — put it after `isengard-proto`).

- [ ] **Step 3: Create the `isengard-storage` crate skeleton**

```bash
cd ~/Projects/isengard
mkdir -p crates/isengard-storage/src crates/isengard-storage/migrations
cat > crates/isengard-storage/Cargo.toml <<'EOF'
[package]
name = "isengard-storage"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "SQLite-backed inventory for Isengard controller"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
sqlx = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
ulid = { workspace = true }

[dev-dependencies]
tempfile = "3.14.0"
EOF

cat > crates/isengard-storage/src/lib.rs <<'EOF'
//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod host;
pub mod inventory;

pub use error::{Error, Result};
pub use host::{EnrollHost, Host, HostId};
pub use inventory::Inventory;
EOF
```

- [ ] **Step 4: Verify the workspace still builds**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
```

Expected: clean build (`isengard-storage` compiles to an empty rlib because lib.rs references modules that don't exist yet — actually that WILL fail).

If `cargo build` fails because `error`, `host`, `inventory` modules don't exist yet, that's fine — the next tasks add them. Temporarily change `lib.rs` to:

```rust
//! Placeholder; populated through Tasks 2-5.
```

…and remove the module declarations + re-exports until Task 2 puts them back. Or, equivalently, write all module files as empty stubs in this task.

Actually, the cleanest fix: write empty stub files for all three modules now. They get content in Tasks 2-4.

```bash
cat > crates/isengard-storage/src/error.rs <<'EOF'
//! Populated in Task 2.
EOF
cat > crates/isengard-storage/src/host.rs <<'EOF'
//! Populated in Task 2.
EOF
cat > crates/isengard-storage/src/inventory.rs <<'EOF'
//! Populated in Tasks 3-4.
EOF
```

- [ ] **Step 5: Re-verify build is clean**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -3
```

Expected: clean. The `pub use` lines in lib.rs reference items that don't exist, so adjust lib.rs to NOT re-export anything yet:

```bash
cat > crates/isengard-storage/src/lib.rs <<'EOF'
//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod host;
pub mod inventory;
EOF
```

`cargo build --workspace` should now succeed.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-storage/ && git commit -m "chore(deps): add sqlx + ulid; scaffold isengard-storage crate"
```

---

## Task 2: Error type + Host types

**Files:**
- Modify: `crates/isengard-storage/src/error.rs`
- Modify: `crates/isengard-storage/src/host.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

This task lands the public types (no SQL yet). Tests inline.

- [ ] **Step 1: Write `error.rs`**

```bash
cat > crates/isengard-storage/src/error.rs <<'EOF'
//! Errors emitted by the storage layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("decoding host row: {reason}")]
    Decode { reason: String },

    #[error("invalid HostId byte length: expected 16, got {0}")]
    InvalidHostId(usize),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_host_id_renders_clearly() {
        let err = Error::InvalidHostId(8);
        assert_eq!(err.to_string(), "invalid HostId byte length: expected 16, got 8");
    }

    #[test]
    fn decode_renders_clearly() {
        let err = Error::Decode { reason: "missing column".into() };
        assert_eq!(err.to_string(), "decoding host row: missing column");
    }
}
EOF
```

- [ ] **Step 2: Write `host.rs`**

```bash
cat > crates/isengard-storage/src/host.rs <<'EOF'
//! Host row and the request type for inserting a new one.

use serde::{Deserialize, Serialize};

/// Stable, monotonic, unique host identifier. Wraps the 16-byte form of a
/// [`ulid::Ulid`] for compact SQLite storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(pub ulid::Ulid);

impl HostId {
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_bytes()
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(ulid::Ulid::from_bytes(bytes))
    }
}

impl Default for HostId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One row from the `hosts` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: HostId,
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
    /// Unix seconds when the host first enrolled.
    pub enrolled_at: i64,
    /// Unix seconds of the last heartbeat. `None` until first heartbeat lands.
    pub last_seen_at: Option<i64>,
    /// Free-form JSON metadata. Defaults to `{}`.
    pub metadata: serde_json::Value,
}

/// Request shape for inserting a new host. The controller calls this; the
/// storage layer assigns a fresh [`HostId`] and `enrolled_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollHost {
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_round_trips_through_bytes() {
        let id = HostId::new();
        let bytes = id.to_bytes();
        let back = HostId::from_bytes(bytes);
        assert_eq!(id, back);
    }

    #[test]
    fn host_id_displays_as_ulid() {
        let id = HostId::from_bytes([0; 16]);
        assert_eq!(id.to_string(), "00000000000000000000000000");
    }

    #[test]
    fn host_id_round_trips_through_json() {
        let id = HostId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: HostId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }
}
EOF
```

- [ ] **Step 3: Re-export from lib.rs**

```bash
cat > crates/isengard-storage/src/lib.rs <<'EOF'
//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod host;
pub mod inventory;

pub use error::{Error, Result};
pub use host::{EnrollHost, Host, HostId};
EOF
```

- [ ] **Step 4: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage 2>&1 | tail -10
```

Expected: 5 tests pass (2 error + 3 host).

- [ ] **Step 5: Run clippy**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-storage --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/src/ && git commit -m "feat(storage): Error + HostId + Host + EnrollHost types"
```

---

## Task 3: Migration + `Inventory::open`

**Files:**
- Create: `crates/isengard-storage/migrations/0001_hosts.sql`
- Modify: `crates/isengard-storage/src/inventory.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write the migration**

```bash
cat > crates/isengard-storage/migrations/0001_hosts.sql <<'EOF'
CREATE TABLE hosts (
    id              BLOB     PRIMARY KEY,
    fingerprint     TEXT     NOT NULL UNIQUE,
    hostname        TEXT     NOT NULL,
    os              TEXT     NOT NULL,
    arch            TEXT     NOT NULL,
    agent_version   TEXT     NOT NULL,
    docker_version  TEXT     NOT NULL,
    enrolled_at     INTEGER  NOT NULL,
    last_seen_at    INTEGER,
    metadata        TEXT     NOT NULL DEFAULT '{}'
);

CREATE INDEX hosts_last_seen_at_idx ON hosts(last_seen_at DESC);
EOF
```

- [ ] **Step 2: Implement `Inventory::open`**

```bash
cat > crates/isengard-storage/src/inventory.rs <<'EOF'
//! `Inventory`: the public CRUD surface over the `hosts` table.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::SqlitePool;

use crate::error::Result;

/// Wraps a `sqlx::SqlitePool` opened against a single `.db` file.
/// Cheap to clone (the pool is `Arc`-backed inside).
#[derive(Debug, Clone)]
pub struct Inventory {
    pool: SqlitePool,
}

impl Inventory {
    /// Open (or create) the database at `path` and run all pending migrations.
    /// The parent directory must exist; the file is created if missing.
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(opts).await?;
        sqlx::migrate!().run(&pool).await?;

        Ok(Self { pool })
    }

    /// Open an in-memory database. Useful for tests; the data is wiped when
    /// the `Inventory` is dropped.
    pub async fn open_in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    /// Borrow the underlying pool. Used by inventory methods (and tests that
    /// want to peek at table state).
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn open_creates_file_and_runs_migrations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let inv = Inventory::open(&path).await.expect("open");
        assert!(path.exists(), "db file should be created");

        // Migration should have created the hosts table — check by querying
        // sqlite_master (sqlite's catalog).
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hosts'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "hosts table should exist after migrate");
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let inv = Inventory::open_in_memory().await.expect("open");
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='hosts_last_seen_at_idx'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "last_seen_at index should exist");
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let _inv1 = Inventory::open(&path).await.expect("open 1");
        // Reopen the same file — migrations should be a no-op the second time.
        let _inv2 = Inventory::open(&path).await.expect("open 2");
    }
}
EOF
```

- [ ] **Step 3: Re-export `Inventory` from lib.rs**

```bash
cat > crates/isengard-storage/src/lib.rs <<'EOF'
//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod error;
pub mod host;
pub mod inventory;

pub use error::{Error, Result};
pub use host::{EnrollHost, Host, HostId};
pub use inventory::Inventory;
EOF
```

- [ ] **Step 4: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage 2>&1 | tail -10
```

Expected: 8 tests pass (5 from Task 2 + 3 inventory).

If sqlx complains about the migrations directory not being found at compile time, the `sqlx::migrate!()` macro defaults to `./migrations` relative to the crate root — that's where the migration file lives, so it should Just Work. If it fails with a path error, double-check the migration path.

- [ ] **Step 5: Run clippy**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-storage --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/migrations/ crates/isengard-storage/src/ Cargo.lock && git commit -m "feat(storage): 0001_hosts migration + Inventory::open with WAL + tests"
```

---

## Task 4: `enroll_host` and `get_host`

**Files:**
- Modify: `crates/isengard-storage/src/inventory.rs`

This task lands write-then-read: insert a host, look it back up by id.

- [ ] **Step 1: Add the methods**

Open `crates/isengard-storage/src/inventory.rs` and append the following inside `impl Inventory { ... }` (above the `pub(crate) fn pool` line is fine):

```rust
    /// Insert a new host. Returns the freshly assigned `HostId`. The
    /// `enrolled_at` timestamp is set to "now" (Unix seconds).
    pub async fn enroll_host(&self, req: EnrollHost) -> Result<HostId> {
        let id = HostId::new();
        let id_bytes: &[u8] = &id.to_bytes();
        let enrolled_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        sqlx::query(
            r#"
            INSERT INTO hosts (
                id, fingerprint, hostname, os, arch,
                agent_version, docker_version, enrolled_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id_bytes)
        .bind(&req.fingerprint)
        .bind(&req.hostname)
        .bind(&req.os)
        .bind(&req.arch)
        .bind(&req.agent_version)
        .bind(&req.docker_version)
        .bind(enrolled_at)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Look up a host by id. Returns `None` if no row matches.
    pub async fn get_host(&self, id: HostId) -> Result<Option<Host>> {
        let id_bytes: &[u8] = &id.to_bytes();

        let row: Option<(
            Vec<u8>, String, String, String, String, String, String, i64, Option<i64>, String,
        )> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata
            FROM hosts
            WHERE id = ?
            "#,
        )
        .bind(id_bytes)
        .fetch_optional(&self.pool)
        .await?;

        row.map(decode_host).transpose()
    }
```

You'll also need to bring [`EnrollHost`] and [`Host`] in scope at the top of the file:

```rust
use crate::host::{EnrollHost, Host, HostId};
```

(also needs `Error` for the decode helper)

```rust
use crate::error::Error;
```

And add a private decode helper at the bottom of the file (outside the `impl Inventory`):

```rust
type HostRow = (
    Vec<u8>, // id
    String,  // fingerprint
    String,  // hostname
    String,  // os
    String,  // arch
    String,  // agent_version
    String,  // docker_version
    i64,     // enrolled_at
    Option<i64>, // last_seen_at
    String,  // metadata (json text)
);

fn decode_host(row: HostRow) -> Result<Host> {
    let id_bytes: [u8; 16] = row.0.as_slice().try_into().map_err(|_| {
        Error::InvalidHostId(row.0.len())
    })?;
    let metadata: serde_json::Value = serde_json::from_str(&row.9).map_err(|e| {
        Error::Decode { reason: format!("metadata json: {e}") }
    })?;

    Ok(Host {
        id: HostId::from_bytes(id_bytes),
        fingerprint: row.1,
        hostname: row.2,
        os: row.3,
        arch: row.4,
        agent_version: row.5,
        docker_version: row.6,
        enrolled_at: row.7,
        last_seen_at: row.8,
        metadata,
    })
}
```

Use the Edit tool to splice these into `inventory.rs`. Final structure: imports at top, `pub struct Inventory`, `impl Inventory { open, open_in_memory, enroll_host, get_host, pool }`, then `fn decode_host`, then `#[cfg(test)] mod tests`.

- [ ] **Step 2: Add tests for the new methods**

In the same `inventory.rs`, find the `#[cfg(test)] mod tests { ... }` block and add these tests inside (alongside the existing 3 tests):

```rust
    fn sample_enrollment() -> EnrollHost {
        EnrollHost {
            fingerprint: "ada-lovelace.example".into(),
            hostname: "ada-lovelace".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0-alpha".into(),
            docker_version: "27.4.0".into(),
        }
    }

    #[tokio::test]
    async fn enroll_then_get_round_trips() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let id = inv.enroll_host(req.clone()).await.unwrap();

        let got = inv.get_host(id).await.unwrap().expect("host should exist");

        assert_eq!(got.id, id);
        assert_eq!(got.fingerprint, req.fingerprint);
        assert_eq!(got.hostname, req.hostname);
        assert_eq!(got.os, req.os);
        assert_eq!(got.arch, req.arch);
        assert_eq!(got.agent_version, req.agent_version);
        assert_eq!(got.docker_version, req.docker_version);
        assert!(got.enrolled_at > 0);
        assert_eq!(got.last_seen_at, None);
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_id() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let result = inv.get_host(HostId::new()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn duplicate_fingerprint_is_rejected() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let _ = inv.enroll_host(req.clone()).await.unwrap();
        let err = inv.enroll_host(req).await.expect_err("dup fingerprint must error");
        // sqlx maps UNIQUE violations to sqlx::Error::Database with kind ERR_*
        // — we just assert it's a Db error variant, not the specific code.
        assert!(matches!(err, Error::Db(_)), "unexpected error: {err:?}");
    }
```

You'll also need `use crate::host::HostId;` and `use crate::error::Error;` at the top of the tests module if not already imported via `use super::*;` — `use super::*;` inherits the parent's imports, so you should be fine.

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage 2>&1 | tail -15
```

Expected: 11 tests pass (5 + 3 + 3 new).

- [ ] **Step 4: Run clippy**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-storage --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/src/inventory.rs && git commit -m "feat(storage): Inventory::enroll_host + get_host with round-trip + dup tests"
```

---

## Task 5: `touch_host` and `list_hosts`

**Files:**
- Modify: `crates/isengard-storage/src/inventory.rs`

- [ ] **Step 1: Add the methods**

Append inside `impl Inventory { ... }`, alongside the methods from Task 4:

```rust
    /// Update `last_seen_at` for a host. No-op if the host doesn't exist.
    /// Returns whether a row was actually updated.
    pub async fn touch_host(&self, id: HostId, ts: i64) -> Result<bool> {
        let id_bytes: &[u8] = &id.to_bytes();
        let result = sqlx::query("UPDATE hosts SET last_seen_at = ? WHERE id = ?")
            .bind(ts)
            .bind(id_bytes)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Return every host, ordered by `last_seen_at DESC` (recently active first;
    /// hosts never seen sort to the bottom because NULL is treated as -infinity
    /// by the index. We explicitly NULLS LAST for clarity.).
    pub async fn list_hosts(&self) -> Result<Vec<Host>> {
        let rows: Vec<HostRow> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata
            FROM hosts
            ORDER BY last_seen_at DESC NULLS LAST, enrolled_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(decode_host).collect()
    }
```

- [ ] **Step 2: Add tests**

Append inside `mod tests { ... }`:

```rust
    #[tokio::test]
    async fn touch_updates_last_seen_for_known_host() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let id = inv.enroll_host(sample_enrollment()).await.unwrap();

        let updated = inv.touch_host(id, 1_700_000_000).await.unwrap();
        assert!(updated);

        let host = inv.get_host(id).await.unwrap().unwrap();
        assert_eq!(host.last_seen_at, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn touch_unknown_host_returns_false() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let updated = inv.touch_host(HostId::new(), 1_700_000_000).await.unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn list_returns_recently_seen_first() {
        let inv = Inventory::open_in_memory().await.unwrap();

        // Enroll two hosts with different fingerprints.
        let mut req_a = sample_enrollment();
        req_a.fingerprint = "host-a.example".into();
        let id_a = inv.enroll_host(req_a).await.unwrap();

        let mut req_b = sample_enrollment();
        req_b.fingerprint = "host-b.example".into();
        let id_b = inv.enroll_host(req_b).await.unwrap();

        // Touch B more recently than A.
        inv.touch_host(id_a, 1_700_000_000).await.unwrap();
        inv.touch_host(id_b, 1_700_000_500).await.unwrap();

        let listed = inv.list_hosts().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, id_b, "more recent host should come first");
        assert_eq!(listed[1].id, id_a);
    }

    #[tokio::test]
    async fn list_empty_inventory_returns_empty_vec() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let listed = inv.list_hosts().await.unwrap();
        assert!(listed.is_empty());
    }
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage 2>&1 | tail -15
```

Expected: 15 tests pass (11 + 4 new).

- [ ] **Step 4: Run clippy**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-storage --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/src/inventory.rs && git commit -m "feat(storage): Inventory::touch_host + list_hosts (recent first)"
```

---

## Task 6: Final CI gate + tag

- [ ] **Step 1: Full local CI**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

Expected: fmt + clippy + tests all pass. Final line `✓ ci-local passed`.

If `cargo fmt --check` fails: `cargo fmt`, then `git add -A && git commit -m "style: cargo fmt across phase 2b crate"`, then re-run `just ci-local`.

- [ ] **Step 2: Verify total test count**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\. [1-9]" | sort -u
```

Expected: lines for ≥ 6 different crates' results, totaling ≥ 27 passing tests. (12 core + 1 controller dummy + 2 server_skeleton + 3 plugin_loading + 1 each tiny crate + 15 storage = ~34.)

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2b -m "phase 2b: isengard-storage crate + Inventory API + hosts table"
cd ~/Projects/isengard && git tag -l | grep phase2b
```

Tag stays local until pushed.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 27 tests green
- [ ] `just ci-local` clean
- [ ] Tag `v0.1.0-alpha.phase2b` exists locally
- [ ] No commits pushed (the phase ends with the local tag; pushing is the human's call)

---

## Self-review

| Spec requirement | Plan task |
|---|---|
| §8 SQLite via sqlx | Task 1 (deps) + Task 3 (open) |
| §8 hosts table schema (12 columns + index) | Task 3 (migration) |
| §8 ULID primary key (16 bytes) | Task 2 (HostId) + Task 3 (BLOB column) |
| §9 isengard-storage crate added to workspace | Task 1 |
| §9 Inventory API: open / enroll_host / touch_host / get_host / list_hosts | Tasks 3-5 |
| §10 unit tests on storage CRUD | Tasks 2-5 (15 unit tests in `isengard-storage`) |
| §11 done state: ≥ 27 tests in workspace | Task 6 |
| §11 tag the sub-phase | Task 6 |

No placeholders; every code step contains real code. All file paths are exact. Every command step has expected output.

---

## Execution Handoff

Plan saved to `docs/superpowers/plans/2026-04-30-phase-2b-storage.md`.

Two execution options as before:

1. **Subagent-Driven (recommended)** — fresh subagent per task with full plan context, review-as-needed, mark complete in TodoWrite.
2. **Inline Execution** — execute in this session.

Subagent-driven is the default given Phase 2a's pattern.
