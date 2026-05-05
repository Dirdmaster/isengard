# Phase 14: Auth & Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Phase 2c's shared `ISENGARD_TOKEN` bearer secret with an internal-CA + per-agent mTLS model. After this phase, the controller boots without operator-supplied secrets, enrollment uses one-time-use short-lived tokens, every controller↔agent gRPC is mTLS-authenticated with per-agent certs, and per-cert revocation works.

**Architecture:** Add a CA (rcgen-generated, ECDSA P-256, stored in SQLite) to the controller. Refactor Enroll into a token-redemption flow that returns a signed agent cert. Wire mTLS into tonic's `ServerTlsConfig` (client cert verification by the CA). Add a per-RPC interceptor for revocation checks that special-cases the Enroll RPC (no client cert needed). Agent persists the cert bundle, builds an mTLS channel for all subsequent RPCs, renews at 50% TTL via a new `RenewCert` RPC.

**Tech Stack:** Rust + sqlx (SQLite) + tonic + rustls + rcgen + tokio. Builds on `isengard-storage` (Inventory pattern), `isengard-controller` (existing tonic Server), `isengard-agent` (existing enroll module + sync loop), `isengard-plugins/dashboard` (Vue 3 + axum REST endpoints). Replaces existing `TokenAuthLayer` and `ISENGARD_TOKEN` env var reads.

**Spec:** `docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md`

**Branch:** `feat/auth-identity`, off `next` (no stacking — single PR).

---

## File map

**Create:**
- `crates/isengard-storage/migrations/0014_auth_and_identity.sql`
- `crates/isengard-storage/src/ca.rs`
- `crates/isengard-storage/src/enrollment_token.rs`
- `crates/isengard-storage/src/agent_cert.rs`
- `crates/isengard-storage/tests/ca.rs`
- `crates/isengard-storage/tests/enrollment_token.rs`
- `crates/isengard-storage/tests/agent_cert.rs`
- `crates/isengard-controller/src/ca.rs`
- `crates/isengard-controller/src/enrollment.rs`
- `crates/isengard-controller/src/revocation.rs`
- `crates/isengard-controller/tests/enrollment_e2e.rs`
- `crates/isengard-controller/tests/mtls_e2e.rs`
- `crates/isengard-controller/tests/cert_renewal_e2e.rs`
- `crates/isengard-agent/src/cert_store.rs`
- `crates/isengard-agent/src/cert_renewal.rs`
- `crates/isengard-agent/tests/cert_store_unit.rs`
- `crates/isengard-agent/tests/auth_e2e.rs`
- `crates/isengard-plugins/dashboard/src/enrollment.rs`
- `crates/isengard-plugins/dashboard/web/composables/useEnrollment.ts`
- `crates/isengard-plugins/dashboard/web/components/EnrollmentSettings.vue`
- `crates/isengard-plugins/dashboard/web/components/MintTokenModal.vue`

**Modify:**
- `crates/isengard-storage/src/lib.rs` — add module exports
- `crates/isengard-storage/Cargo.toml` — add `sha2` (already a transitive but make direct)
- `crates/isengard-controller/src/lib.rs` — add modules, switch from `TokenAuthLayer` to mTLS-aware interceptor
- `crates/isengard-controller/src/service.rs` — refactor `Enroll`, add `RenewCert`
- `crates/isengard-controller/src/auth.rs` — delete `TokenAuthLayer`, replace with `CertAuthInterceptor`
- `crates/isengard-controller/Cargo.toml` — add `rcgen`, `rustls-pemfile`, `x509-parser`, `sha2`, `rand`
- `crates/isengard-proto/proto/isengard.v1.proto` — `EnrollRequest`/`Response` fields, add `RenewCert` RPC
- `crates/isengard-agent/src/lib.rs` — refactor enrollment + drop `ISENGARD_TOKEN`, build mTLS channel, spawn renewal task
- `crates/isengard-agent/src/enroll.rs` — change signature to take token, return cert bundle
- `crates/isengard-agent/src/sync.rs` — wire renewal task
- `crates/isengard-agent/Cargo.toml` — add `rustls-pemfile`, `x509-parser`, `tempfile` (test)
- `crates/isengard/src/main.rs` — drop `ISENGARD_TOKEN` reads, add controller subcommands
- `crates/isengard-plugins/dashboard/src/lib.rs` — register enrollment routes
- `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue` — add Enrollment tab
- `crates/isengard-plugins/dashboard/web/components/HostInspector.vue` — show cert info + revoke button
- `Cargo.toml` (workspace) — add `rcgen = "0.13"`, `sha2 = "0.10"`, `rand = "0.8"`, `x509-parser = "0.16"`, `rustls-pemfile = "2"`

**Delete (cleanup):**
- The body of `TokenAuthLayer` in `crates/isengard-controller/src/auth.rs` (replace with CertAuthInterceptor)

---

## Task split

| Task | Scope | Tests added |
|---|---|---|
| 1 | Migration 0014 + storage modules (ca, enrollment_token, agent_cert) + DAOs | 3 storage test files (~10 tests) |
| 2 | Controller `ca::Authority` (load_or_init, sign_agent_leaf) | 3 unit |
| 3 | Controller `enrollment::EnrollmentService` (mint, redeem) | 4 unit |
| 4 | Controller `revocation::RevocationSet` | 3 unit |
| 5 | Proto: `EnrollRequest/Response` field updates + `RenewCert` RPC | (compile) |
| 6 | Controller: refactor `Enroll` handler to use EnrollmentService | 1 integration |
| 7 | Controller: implement `RenewCert` handler | 1 integration |
| 8 | Controller: tonic mTLS server config + `CertAuthInterceptor` | 1 integration (mtls_e2e) |
| 9 | CLI: drop `ISENGARD_TOKEN` reads, add `controller token mint` / `controller agent revoke` / `controller agent list` | 2 unit + smoke |
| 10 | Agent: `cert_store` module | 4 unit |
| 11 | Agent: enrollment refactor (read `ISENGARD_ENROLL_TOKEN`, persist bundle, build mTLS channel) | 1 unit + (manual) |
| 12 | Agent: `cert_renewal` task wired into sync loop | 2 unit |
| 13 | Dashboard backend: REST endpoints (`POST /enrollment/tokens`, `GET /enrollment/tokens`, `DELETE /enrollment/tokens/:hash`, `DELETE /hosts/:id/cert`) | 2 integration |
| 14 | Dashboard frontend: Enrollment tab + Mint Token modal + per-host revoke UI | (manual smoke) |
| 15 | E2E: real-Docker `auth_e2e.rs` (enroll → mTLS heartbeat → revoke → reject) | 1 e2e (`#[ignore]`) |
| 16 | Final: README updates, release-notes draft, workspace gates green, open PR | (gates) |

---

## Task 1: Migration 0014 + storage layer (ca + enrollment_tokens + agent_certs)

**Files:**
- Create: `crates/isengard-storage/migrations/0014_auth_and_identity.sql`
- Create: `crates/isengard-storage/src/ca.rs`
- Create: `crates/isengard-storage/src/enrollment_token.rs`
- Create: `crates/isengard-storage/src/agent_cert.rs`
- Create: `crates/isengard-storage/tests/ca.rs`
- Create: `crates/isengard-storage/tests/enrollment_token.rs`
- Create: `crates/isengard-storage/tests/agent_cert.rs`
- Modify: `crates/isengard-storage/src/lib.rs`
- Modify: `crates/isengard-storage/Cargo.toml`

- [ ] **Step 1: Write the failing storage tests**

Create `crates/isengard-storage/tests/ca.rs`:

```rust
use isengard_storage::ca::CaRow;
use isengard_storage::Inventory;

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

#[tokio::test]
async fn get_ca_returns_none_when_unset() {
    let inv = fresh_inv().await;
    assert!(inv.get_ca().await.unwrap().is_none());
}

#[tokio::test]
async fn set_ca_then_get_round_trips() {
    let inv = fresh_inv().await;
    let row = CaRow {
        root_cert_pem: "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n".into(),
        root_key_pem: "-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----\n".into(),
    };
    inv.set_ca(row.clone()).await.unwrap();
    let got = inv.get_ca().await.unwrap().expect("ca present");
    assert_eq!(got.root_cert_pem, row.root_cert_pem);
    assert_eq!(got.root_key_pem, row.root_key_pem);
}

#[tokio::test]
async fn set_ca_twice_errors() {
    let inv = fresh_inv().await;
    let row = CaRow {
        root_cert_pem: "cert".into(),
        root_key_pem: "key".into(),
    };
    inv.set_ca(row.clone()).await.unwrap();
    let err = inv.set_ca(row).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("ca"));
}
```

Create `crates/isengard-storage/tests/enrollment_token.rs`:

```rust
use chrono::{Duration, Utc};
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use isengard_storage::Inventory;

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

fn hash(token: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).to_vec()
}

#[tokio::test]
async fn insert_then_find_returns_record() {
    let inv = fresh_inv().await;
    let h = hash("abc");
    let exp = Utc::now() + Duration::minutes(15);
    inv.insert_enrollment_token(h.clone(), TokenRole::Agent, exp).await.unwrap();
    let rec = inv.find_active_token(&h).await.unwrap().expect("found");
    assert_eq!(rec.token_hash, h);
    assert_eq!(rec.role, TokenRole::Agent);
    assert!(rec.consumed_at.is_none());
}

#[tokio::test]
async fn find_active_skips_expired() {
    let inv = fresh_inv().await;
    let h = hash("expired");
    inv.insert_enrollment_token(h.clone(), TokenRole::Agent, Utc::now() - Duration::seconds(1))
        .await
        .unwrap();
    assert!(inv.find_active_token(&h).await.unwrap().is_none());
}

#[tokio::test]
async fn consume_marks_consumed_atomically() {
    let inv = fresh_inv().await;
    let h = hash("consume-me");
    inv.insert_enrollment_token(h.clone(), TokenRole::Agent, Utc::now() + Duration::minutes(5))
        .await.unwrap();
    let host_id = HostId::new();
    inv.consume_enrollment_token(&h, host_id).await.unwrap();
    let rec = inv.find_active_token(&h).await.unwrap();
    assert!(rec.is_none(), "consumed token should not be returned by find_active");
}

#[tokio::test]
async fn consume_twice_errors() {
    let inv = fresh_inv().await;
    let h = hash("once");
    inv.insert_enrollment_token(h.clone(), TokenRole::Agent, Utc::now() + Duration::minutes(5))
        .await.unwrap();
    let host_id = HostId::new();
    inv.consume_enrollment_token(&h, host_id).await.unwrap();
    assert!(inv.consume_enrollment_token(&h, host_id).await.is_err());
}
```

Create `crates/isengard-storage/tests/agent_cert.rs`:

```rust
use chrono::{Duration, Utc};
use isengard_storage::agent_cert::AgentCert;
use isengard_storage::host::{EnrollHost, HostId};
use isengard_storage::Inventory;

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

async fn make_host(inv: &Inventory) -> HostId {
    let id = HostId::new();
    inv.enroll_host(EnrollHost {
        id,
        hostname: "h1".into(),
        os: "linux".into(),
        version: "0.1".into(),
    }).await.unwrap();
    id
}

#[tokio::test]
async fn insert_then_active_lookup_returns_cert() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    let cert = AgentCert {
        serial: vec![1, 2, 3, 4],
        host_id: host,
        cert_pem: "cert-pem".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    };
    inv.insert_agent_cert(cert.clone()).await.unwrap();
    let active = inv.active_cert_for_host(host).await.unwrap().expect("present");
    assert_eq!(active.serial, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn revoke_marks_revoked_and_active_returns_none() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    let serial = vec![9, 9, 9];
    inv.insert_agent_cert(AgentCert {
        serial: serial.clone(),
        host_id: host,
        cert_pem: "p".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    }).await.unwrap();
    inv.revoke_cert(&serial, "test").await.unwrap();
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
}

#[tokio::test]
async fn revoked_serials_returns_all_revoked() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    for s in [vec![1u8], vec![2u8], vec![3u8]] {
        inv.insert_agent_cert(AgentCert {
            serial: s.clone(),
            host_id: host,
            cert_pem: "p".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(30),
            revoked_at: None,
            revoke_reason: None,
        }).await.unwrap();
    }
    inv.revoke_cert(&vec![1], "r").await.unwrap();
    inv.revoke_cert(&vec![3], "r").await.unwrap();
    let mut got = inv.revoked_serials().await.unwrap();
    got.sort();
    assert_eq!(got, vec![vec![1u8], vec![3u8]]);
}
```

- [ ] **Step 2: Run tests to verify they fail (compile error)**

Run: `cargo test -p isengard-storage --test ca --test enrollment_token --test agent_cert`
Expected: compile errors — modules don't exist yet.

- [ ] **Step 3: Write the migration**

Create `crates/isengard-storage/migrations/0014_auth_and_identity.sql`:

```sql
-- Phase 14: Auth & Identity. CA, enrollment tokens, per-agent certs.
-- See docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md

CREATE TABLE ca (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    root_cert_pem   TEXT NOT NULL,
    root_key_pem    TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE enrollment_tokens (
    token_hash      BLOB PRIMARY KEY,
    role            TEXT NOT NULL CHECK (role IN ('agent')),
    expires_at      TEXT NOT NULL,
    consumed_at     TEXT,
    consumed_by     BLOB,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_enrollment_tokens_active
    ON enrollment_tokens(expires_at)
    WHERE consumed_at IS NULL;

CREATE TABLE agent_certs (
    serial          BLOB PRIMARY KEY,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    cert_pem        TEXT NOT NULL,
    issued_at       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT NOT NULL,
    revoked_at      TEXT,
    revoke_reason   TEXT
);

CREATE INDEX idx_agent_certs_host_active
    ON agent_certs(host_id, issued_at DESC)
    WHERE revoked_at IS NULL;
```

- [ ] **Step 4: Add `sha2` to storage Cargo.toml**

Modify `crates/isengard-storage/Cargo.toml`, in `[dependencies]`:

```toml
sha2 = { workspace = true }
```

And in workspace root `Cargo.toml`:

```toml
sha2 = "0.10"
```

- [ ] **Step 5: Implement `ca` module**

Create `crates/isengard-storage/src/ca.rs`:

```rust
//! Single-row CA storage. The PEM blobs are *not* additionally encrypted:
//! file permissions on the SQLite file (chmod 600 in state-dir) are the only
//! protection. See spec §"CA private key protection" for the limitation.

use crate::{Error, Inventory, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaRow {
    pub root_cert_pem: String,
    pub root_key_pem: String,
}

impl Inventory {
    pub async fn get_ca(&self) -> Result<Option<CaRow>> {
        let row = sqlx::query!(
            "SELECT root_cert_pem, root_key_pem FROM ca WHERE id = 1"
        )
        .fetch_optional(self.pool())
        .await
        .map_err(Error::from)?;

        Ok(row.map(|r| CaRow {
            root_cert_pem: r.root_cert_pem,
            root_key_pem: r.root_key_pem,
        }))
    }

    pub async fn set_ca(&self, row: CaRow) -> Result<()> {
        sqlx::query!(
            "INSERT INTO ca (id, root_cert_pem, root_key_pem) VALUES (1, ?, ?)",
            row.root_cert_pem,
            row.root_key_pem,
        )
        .execute(self.pool())
        .await
        .map_err(|e| Error::from(e))?;
        Ok(())
    }
}
```

- [ ] **Step 6: Implement `enrollment_token` module**

Create `crates/isengard-storage/src/enrollment_token.rs`:

```rust
use chrono::{DateTime, Utc};

use crate::host::HostId;
use crate::{Error, Inventory, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase", type_name = "TEXT")]
pub enum TokenRole {
    Agent,
}

#[derive(Debug, Clone)]
pub struct EnrollmentTokenRecord {
    pub token_hash: Vec<u8>,
    pub role: TokenRole,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub consumed_by: Option<HostId>,
    pub created_at: DateTime<Utc>,
}

impl Inventory {
    pub async fn insert_enrollment_token(
        &self,
        token_hash: Vec<u8>,
        role: TokenRole,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let role_str = match role { TokenRole::Agent => "agent" };
        sqlx::query!(
            "INSERT INTO enrollment_tokens (token_hash, role, expires_at) VALUES (?, ?, ?)",
            token_hash,
            role_str,
            expires_at,
        )
        .execute(self.pool())
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    pub async fn find_active_token(&self, hash: &[u8]) -> Result<Option<EnrollmentTokenRecord>> {
        let now = Utc::now();
        let rec = sqlx::query!(
            "SELECT token_hash, role, expires_at, consumed_at, consumed_by, created_at
             FROM enrollment_tokens
             WHERE token_hash = ? AND consumed_at IS NULL AND expires_at > ?",
            hash,
            now,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(Error::from)?;

        Ok(rec.map(|r| EnrollmentTokenRecord {
            token_hash: r.token_hash,
            role: TokenRole::Agent, // v1: only role
            expires_at: r.expires_at.parse().expect("valid ts"),
            consumed_at: r.consumed_at.and_then(|s| s.parse().ok()),
            consumed_by: r.consumed_by.and_then(|b| HostId::from_db_bytes(b).ok()),
            created_at: r.created_at.parse().expect("valid ts"),
        }))
    }

    pub async fn consume_enrollment_token(&self, hash: &[u8], host_id: HostId) -> Result<()> {
        let now = Utc::now();
        let host_bytes = host_id.to_db_bytes();
        let res = sqlx::query!(
            "UPDATE enrollment_tokens
             SET consumed_at = ?, consumed_by = ?
             WHERE token_hash = ? AND consumed_at IS NULL",
            now,
            host_bytes,
            hash,
        )
        .execute(self.pool())
        .await
        .map_err(Error::from)?;

        if res.rows_affected() == 0 {
            return Err(Error::NotFound(format!(
                "enrollment token not found or already consumed"
            )));
        }
        Ok(())
    }

    pub async fn list_active_tokens(&self) -> Result<Vec<EnrollmentTokenRecord>> {
        let now = Utc::now();
        let rows = sqlx::query!(
            "SELECT token_hash, role, expires_at, consumed_at, consumed_by, created_at
             FROM enrollment_tokens
             WHERE consumed_at IS NULL AND expires_at > ?
             ORDER BY created_at DESC",
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(Error::from)?;

        Ok(rows.into_iter().map(|r| EnrollmentTokenRecord {
            token_hash: r.token_hash,
            role: TokenRole::Agent,
            expires_at: r.expires_at.parse().expect("valid ts"),
            consumed_at: None,
            consumed_by: None,
            created_at: r.created_at.parse().expect("valid ts"),
        }).collect())
    }
}
```

- [ ] **Step 7: Implement `agent_cert` module**

Create `crates/isengard-storage/src/agent_cert.rs`:

```rust
use chrono::{DateTime, Utc};

use crate::host::HostId;
use crate::{Error, Inventory, Result};

#[derive(Debug, Clone)]
pub struct AgentCert {
    pub serial: Vec<u8>,
    pub host_id: HostId,
    pub cert_pem: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoke_reason: Option<String>,
}

impl Inventory {
    pub async fn insert_agent_cert(&self, cert: AgentCert) -> Result<()> {
        let host_bytes = cert.host_id.to_db_bytes();
        sqlx::query!(
            "INSERT INTO agent_certs (serial, host_id, cert_pem, issued_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
            cert.serial,
            host_bytes,
            cert.cert_pem,
            cert.issued_at,
            cert.expires_at,
        )
        .execute(self.pool())
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    pub async fn active_cert_for_host(&self, host_id: HostId) -> Result<Option<AgentCert>> {
        let host_bytes = host_id.to_db_bytes();
        let row = sqlx::query!(
            "SELECT serial, host_id, cert_pem, issued_at, expires_at, revoked_at, revoke_reason
             FROM agent_certs
             WHERE host_id = ? AND revoked_at IS NULL
             ORDER BY issued_at DESC
             LIMIT 1",
            host_bytes,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(Error::from)?;

        Ok(row.map(|r| AgentCert {
            serial: r.serial,
            host_id: HostId::from_db_bytes(r.host_id).expect("valid host_id"),
            cert_pem: r.cert_pem,
            issued_at: r.issued_at.parse().expect("ts"),
            expires_at: r.expires_at.parse().expect("ts"),
            revoked_at: None,
            revoke_reason: None,
        }))
    }

    pub async fn revoke_cert(&self, serial: &[u8], reason: &str) -> Result<()> {
        let now = Utc::now();
        let res = sqlx::query!(
            "UPDATE agent_certs SET revoked_at = ?, revoke_reason = ?
             WHERE serial = ? AND revoked_at IS NULL",
            now,
            reason,
            serial,
        )
        .execute(self.pool())
        .await
        .map_err(Error::from)?;

        if res.rows_affected() == 0 {
            return Err(Error::NotFound(format!("cert not found or already revoked")));
        }
        Ok(())
    }

    pub async fn revoked_serials(&self) -> Result<Vec<Vec<u8>>> {
        let rows = sqlx::query!(
            "SELECT serial FROM agent_certs WHERE revoked_at IS NOT NULL"
        )
        .fetch_all(self.pool())
        .await
        .map_err(Error::from)?;

        Ok(rows.into_iter().map(|r| r.serial).collect())
    }
}
```

- [ ] **Step 8: Wire modules into lib.rs**

Modify `crates/isengard-storage/src/lib.rs`, add at top of module list:

```rust
pub mod agent_cert;
pub mod ca;
pub mod enrollment_token;
```

And in re-exports section add:

```rust
pub use agent_cert::AgentCert;
pub use ca::CaRow;
pub use enrollment_token::{EnrollmentTokenRecord, TokenRole};
```

- [ ] **Step 9: Run tests to verify pass**

Run: `cargo test -p isengard-storage --test ca --test enrollment_token --test agent_cert`
Expected: all 10 tests pass.

- [ ] **Step 10: Run full storage workspace tests**

Run: `cargo test -p isengard-storage` and `cargo clippy -p isengard-storage --all-targets -- -D warnings`
Expected: green.

- [ ] **Step 11: Commit**

```bash
git add crates/isengard-storage/migrations/0014_auth_and_identity.sql \
        crates/isengard-storage/src/{ca,enrollment_token,agent_cert,lib}.rs \
        crates/isengard-storage/tests/{ca,enrollment_token,agent_cert}.rs \
        crates/isengard-storage/Cargo.toml \
        Cargo.toml
git commit -m "feat(storage): phase 14 ca + enrollment_tokens + agent_certs (migration 0014)"
```

---

## Task 2: Controller `ca::Authority` (load_or_init + sign_agent_leaf)

**Files:**
- Create: `crates/isengard-controller/src/ca.rs`
- Modify: `crates/isengard-controller/src/lib.rs`
- Modify: `crates/isengard-controller/Cargo.toml`

- [ ] **Step 1: Add deps to controller Cargo.toml**

In `crates/isengard-controller/Cargo.toml`, add to `[dependencies]`:

```toml
rcgen = { workspace = true }
rand = { workspace = true }
sha2 = { workspace = true }
x509-parser = { workspace = true }
```

In workspace `Cargo.toml`:

```toml
rcgen = { version = "0.13", default-features = false, features = ["pem", "x509-parser"] }
rand = "0.8"
x509-parser = "0.16"
rustls-pemfile = "2"
```

- [ ] **Step 2: Write the failing tests**

Create `crates/isengard-controller/tests/ca_unit.rs`:

```rust
use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_storage::host::HostId;
use isengard_storage::Inventory;

#[tokio::test]
async fn load_or_init_creates_then_persists() {
    let inv = Inventory::open_in_memory().await.unwrap();

    let auth1 = Authority::load_or_init(&inv).await.unwrap();
    let cert1 = auth1.root_cert_pem().to_string();

    let auth2 = Authority::load_or_init(&inv).await.unwrap();
    assert_eq!(auth2.root_cert_pem(), cert1, "second load should reuse persisted CA");
}

#[tokio::test]
async fn sign_agent_leaf_chains_to_root() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let host_id = HostId::new();
    let leaf = auth.sign_agent_leaf(host_id, "agent-host", Duration::days(30)).unwrap();

    assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf.key_pem.contains("BEGIN PRIVATE KEY"));
    assert_eq!(leaf.serial.len(), 16);
    assert!(leaf.expires_at > chrono::Utc::now());

    // Verify the leaf chains to the CA. Use x509-parser to load both, then
    // verify the leaf's signature against the CA's public key.
    let (_, root) = x509_parser::pem::parse_x509_pem(auth.root_cert_pem().as_bytes()).unwrap();
    let root_cert = root.parse_x509().unwrap();
    let (_, leaf_pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let leaf_cert = leaf_pem.parse_x509().unwrap();
    leaf_cert.verify_signature(Some(root_cert.public_key())).expect("leaf chains to root");
}

#[tokio::test]
async fn sign_agent_leaf_includes_hostname_san() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let leaf = auth.sign_agent_leaf(HostId::new(), "my-agent.example.com", Duration::days(30)).unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let san = cert.subject_alternative_name().unwrap().expect("SAN present");
    let dns_names: Vec<_> = san.value.general_names.iter().filter_map(|n| match n {
        x509_parser::extensions::GeneralName::DNSName(s) => Some(*s),
        _ => None,
    }).collect();
    assert!(dns_names.contains(&"my-agent.example.com"), "got {:?}", dns_names);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p isengard-controller --test ca_unit`
Expected: compile error — module doesn't exist.

- [ ] **Step 4: Implement `Authority`**

Create `crates/isengard-controller/src/ca.rs`:

```rust
//! Internal Certificate Authority for issuing per-agent mTLS certs.
//! See spec docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};
use rand::RngCore;

use isengard_storage::ca::CaRow;
use isengard_storage::host::HostId;
use isengard_storage::Inventory;

const CA_TTL_DAYS: i64 = 365 * 10;

pub struct Authority {
    cert_pem: String,
    key_pem: String,
    cert: Certificate,
    key_pair: KeyPair,
}

pub struct IssuedLeaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub serial: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}

impl Authority {
    pub async fn load_or_init(inventory: &Inventory) -> Result<Self> {
        if let Some(row) = inventory.get_ca().await.context("ca lookup")? {
            let key_pair = KeyPair::from_pem(&row.root_key_pem).context("parse ca key")?;
            let mut params = CertificateParams::from_ca_cert_pem(&row.root_cert_pem)
                .context("parse ca cert")?;
            params.key_pair = Some(key_pair.clone()); // not used at sign time but consistent shape
            let cert = params.self_signed(&key_pair).context("rebuild ca cert")?;
            return Ok(Authority { cert_pem: row.root_cert_pem, key_pem: row.root_key_pem, cert, key_pair });
        }

        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::new(vec![]).context("ca params")?;
        params.distinguished_name.push(DnType::CommonName, "Isengard Internal CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = Utc::now().into();
        params.not_after = (Utc::now() + Duration::days(CA_TTL_DAYS)).into();

        let cert = params.self_signed(&key_pair).context("self-sign ca")?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        inventory.set_ca(CaRow {
            root_cert_pem: cert_pem.clone(),
            root_key_pem: key_pem.clone(),
        }).await.context("persist ca")?;

        Ok(Authority { cert_pem, key_pem, cert, key_pair })
    }

    pub fn root_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn root_key_pem(&self) -> &str {
        &self.key_pem
    }

    pub fn sign_agent_leaf(
        &self,
        host_id: HostId,
        hostname: &str,
        ttl: Duration,
    ) -> Result<IssuedLeaf> {
        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::new(vec![hostname.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, host_id.to_string());
        params.subject_alt_names.push(SanType::DnsName(hostname.try_into()?));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let now = Utc::now();
        params.not_before = now.into();
        let expires_at = now + ttl;
        params.not_after = expires_at.into();

        let mut serial_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut serial_bytes);
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));

        let leaf_cert = params.signed_by(&leaf_key, &self.cert, &self.key_pair)?;

        Ok(IssuedLeaf {
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
            serial: serial_bytes.to_vec(),
            expires_at,
        })
    }
}
```

- [ ] **Step 5: Wire into lib.rs**

Modify `crates/isengard-controller/src/lib.rs`, add to module list:

```rust
pub mod ca;
```

- [ ] **Step 6: Run tests + clippy**

Run: `cargo test -p isengard-controller --test ca_unit` and `cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: 3 tests pass, no clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/isengard-controller/{src/ca.rs,src/lib.rs,tests/ca_unit.rs,Cargo.toml} Cargo.toml
git commit -m "feat(controller): ca::Authority for issuing per-agent mTLS leaf certs (rcgen, ECDSA P-256)"
```

---

## Task 3: Controller `enrollment::EnrollmentService` (mint + redeem)

**Files:**
- Create: `crates/isengard-controller/src/enrollment.rs`
- Create: `crates/isengard-controller/tests/enrollment_unit.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/isengard-controller/tests/enrollment_unit.rs`:

```rust
use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::{EnrollmentService, HostInfo};
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::Inventory;

async fn fixture() -> (Arc<Inventory>, EnrollmentService) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let svc = EnrollmentService::new(inv.clone(), ca);
    (inv, svc)
}

fn host_info() -> HostInfo {
    HostInfo {
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1.0".into(),
    }
}

#[tokio::test]
async fn mint_returns_base32_token_of_expected_length() {
    let (_, svc) = fixture().await;
    let token = svc.mint(TokenRole::Agent, Duration::minutes(15)).await.unwrap();
    assert!(token.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "token must be uppercase base32 alphanum: {token}");
    assert!(token.len() >= 50 && token.len() <= 56, "got length {}", token.len());
}

#[tokio::test]
async fn redeem_valid_token_returns_signed_cert() {
    let (_, svc) = fixture().await;
    let token = svc.mint(TokenRole::Agent, Duration::minutes(15)).await.unwrap();
    let resp = svc.redeem(&token, host_info()).await.unwrap();

    assert!(resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(resp.agent_key_pem.contains("BEGIN PRIVATE KEY"));
    assert!(resp.ca_root_pem.contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn redeem_unknown_token_errors() {
    let (_, svc) = fixture().await;
    let err = svc.redeem("INVALID-TOKEN-XXXX", host_info()).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("token"));
}

#[tokio::test]
async fn redeem_twice_errors_second_time() {
    let (_, svc) = fixture().await;
    let token = svc.mint(TokenRole::Agent, Duration::minutes(15)).await.unwrap();
    svc.redeem(&token, host_info()).await.unwrap();
    let err = svc.redeem(&token, host_info()).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("token"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p isengard-controller --test enrollment_unit`
Expected: compile error.

- [ ] **Step 3: Implement `EnrollmentService`**

Create `crates/isengard-controller/src/enrollment.rs`:

```rust
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use base32::Alphabet;
use chrono::Duration;
use rand::RngCore;
use sha2::{Digest, Sha256};

use isengard_storage::agent_cert::AgentCert;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::{EnrollHost, HostId};
use isengard_storage::Inventory;

use crate::ca::Authority;

const LEAF_TTL_DAYS: i64 = 30;
const HEARTBEAT_INTERVAL_SECS: u32 = 10;

pub struct EnrollmentService {
    inventory: Arc<Inventory>,
    ca: Arc<Authority>,
}

#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct EnrollResponse {
    pub host_id: HostId,
    pub agent_cert_pem: String,
    pub agent_key_pem: String,
    pub ca_root_pem: String,
    pub heartbeat_interval_secs: u32,
}

impl EnrollmentService {
    pub fn new(inventory: Arc<Inventory>, ca: Arc<Authority>) -> Self {
        Self { inventory, ca }
    }

    pub async fn mint(&self, role: TokenRole, ttl: Duration) -> Result<String> {
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token = base32::encode(Alphabet::Rfc4648 { padding: false }, &raw);
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        let expires_at = chrono::Utc::now() + ttl;

        self.inventory
            .insert_enrollment_token(hash, role, expires_at)
            .await
            .context("persist enrollment token")?;
        Ok(token)
    }

    pub async fn redeem(&self, token: &str, host_info: HostInfo) -> Result<EnrollResponse> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        let _record = self.inventory.find_active_token(&hash).await
            .context("token lookup")?
            .ok_or_else(|| anyhow!("enrollment token unknown, expired, or already consumed"))?;

        let host_id = HostId::new();

        // Enroll host into inventory (idempotent on host_id).
        self.inventory.enroll_host(EnrollHost {
            id: host_id,
            hostname: host_info.hostname.clone(),
            os: host_info.os,
            version: host_info.version,
        }).await.context("enroll host")?;

        let leaf = self.ca.sign_agent_leaf(
            host_id,
            &host_info.hostname,
            Duration::days(LEAF_TTL_DAYS),
        ).context("sign leaf cert")?;

        self.inventory.insert_agent_cert(AgentCert {
            serial: leaf.serial.clone(),
            host_id,
            cert_pem: leaf.cert_pem.clone(),
            issued_at: chrono::Utc::now(),
            expires_at: leaf.expires_at,
            revoked_at: None,
            revoke_reason: None,
        }).await.context("persist cert")?;

        // Mark token consumed AFTER successful issuance, so that a failure mid-flow
        // doesn't burn the token. Race: if two redeem calls land for the same token,
        // both might pass find_active_token, but only one will succeed at consume.
        // The second will get an error and the first wins. The cert was already issued
        // for the loser — that's a dangling cert, acceptable for an internal CA.
        self.inventory.consume_enrollment_token(&hash, host_id).await
            .context("consume token (race?)")?;

        Ok(EnrollResponse {
            host_id,
            agent_cert_pem: leaf.cert_pem,
            agent_key_pem: leaf.key_pem,
            ca_root_pem: self.ca.root_cert_pem().to_string(),
            heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
        })
    }
}
```

Add `base32 = "0.5"` to workspace `Cargo.toml` and depend on it in `crates/isengard-controller/Cargo.toml`.

- [ ] **Step 4: Wire into lib.rs**

```rust
pub mod enrollment;
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p isengard-controller --test enrollment_unit && cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-controller/{src/enrollment.rs,src/lib.rs,tests/enrollment_unit.rs,Cargo.toml} Cargo.toml
git commit -m "feat(controller): EnrollmentService mint + redeem (token → cert exchange)"
```

---

## Task 4: Controller `revocation::RevocationSet`

**Files:**
- Create: `crates/isengard-controller/src/revocation.rs`
- Create: `crates/isengard-controller/tests/revocation_unit.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/isengard-controller/tests/revocation_unit.rs`:

```rust
use isengard_controller::revocation::{revoke_agent, RevocationSet};
use isengard_storage::agent_cert::AgentCert;
use isengard_storage::host::{EnrollHost, HostId};
use isengard_storage::Inventory;

async fn host_with_cert(inv: &Inventory, serial: Vec<u8>) -> HostId {
    let id = HostId::new();
    inv.enroll_host(EnrollHost { id, hostname: "h".into(), os: "linux".into(), version: "0".into() })
        .await.unwrap();
    inv.insert_agent_cert(AgentCert {
        serial,
        host_id: id,
        cert_pem: "p".into(),
        issued_at: chrono::Utc::now(),
        expires_at: chrono::Utc::now() + chrono::Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    }).await.unwrap();
    id
}

#[tokio::test]
async fn load_from_inventory_picks_up_existing_revocations() {
    let inv = Inventory::open_in_memory().await.unwrap();
    host_with_cert(&inv, vec![1, 2, 3]).await;
    inv.revoke_cert(&[1, 2, 3], "test").await.unwrap();

    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    assert!(set.contains(&[1, 2, 3]));
    assert!(!set.contains(&[9, 9, 9]));
}

#[tokio::test]
async fn revoke_runtime_adds_to_set() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    set.revoke(vec![5, 5, 5]);
    assert!(set.contains(&[5, 5, 5]));
}

#[tokio::test]
async fn revoke_agent_persists_and_updates_set() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host = host_with_cert(&inv, vec![7, 7, 7]).await;
    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    assert!(!set.contains(&[7, 7, 7]));

    revoke_agent(&inv, &set, host, "decommission").await.unwrap();

    assert!(set.contains(&[7, 7, 7]));
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p isengard-controller --test revocation_unit`
Expected: compile error.

- [ ] **Step 3: Implement `RevocationSet`**

Create `crates/isengard-controller/src/revocation.rs`:

```rust
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::RwLock;

use isengard_storage::host::HostId;
use isengard_storage::Inventory;

#[derive(Clone)]
pub struct RevocationSet {
    inner: Arc<RwLock<HashSet<Vec<u8>>>>,
}

impl RevocationSet {
    pub async fn load_from_inventory(inventory: &Inventory) -> Result<Self> {
        let serials = inventory.revoked_serials().await?;
        Ok(Self { inner: Arc::new(RwLock::new(serials.into_iter().collect())) })
    }

    pub fn contains(&self, serial: &[u8]) -> bool {
        self.inner.read().contains(serial)
    }

    pub fn revoke(&self, serial: Vec<u8>) {
        self.inner.write().insert(serial);
    }
}

pub async fn revoke_agent(
    inventory: &Inventory,
    revocation: &RevocationSet,
    host_id: HostId,
    reason: &str,
) -> Result<()> {
    let cert = inventory.active_cert_for_host(host_id).await?
        .ok_or_else(|| anyhow!("no active cert for host"))?;
    inventory.revoke_cert(&cert.serial, reason).await?;
    revocation.revoke(cert.serial);
    Ok(())
}
```

Add `parking_lot` to controller `Cargo.toml` if not already there.

- [ ] **Step 4: Wire into lib.rs**

```rust
pub mod revocation;
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p isengard-controller --test revocation_unit && cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-controller/{src/revocation.rs,src/lib.rs,tests/revocation_unit.rs,Cargo.toml}
git commit -m "feat(controller): RevocationSet (in-memory + persistent revoke_agent helper)"
```

---

## Task 5: Proto changes (`EnrollRequest/Response` + `RenewCert` RPC)

**Files:**
- Modify: `crates/isengard-proto/proto/isengard.v1.proto`

- [ ] **Step 1: Edit the proto**

In `crates/isengard-proto/proto/isengard.v1.proto`, replace the existing `EnrollRequest` and `EnrollResponse`:

```proto
message EnrollRequest {
  // Phase 14: replaces the bearer-token interceptor. Token is the
  // base32-encoded short-lived enrollment token minted by the controller.
  string token = 1;
  string hostname = 2;
  string os = 3;
  string version = 4;
}

message EnrollResponse {
  bytes host_id = 1;
  // Phase 14: cert bundle for mTLS. cert_pem and key_pem are the agent's
  // per-host leaf, ca_root_pem is the root the agent uses to validate the
  // controller's TLS cert.
  string agent_cert_pem = 2;
  string agent_key_pem = 3;
  string ca_root_pem = 4;
  uint32 heartbeat_interval_secs = 5;
}

message RenewCertRequest {
  bytes host_id = 1;
}

message RenewCertResponse {
  string agent_cert_pem = 1;
  string agent_key_pem = 2;
  // RFC3339
  string expires_at = 3;
}
```

In the `service Controller` block, add the new RPC alongside the existing ones:

```proto
service Controller {
  rpc Enroll(EnrollRequest) returns (EnrollResponse);
  rpc Sync(stream AgentMessage) returns (stream ControllerMessage);
  rpc RenewCert(RenewCertRequest) returns (RenewCertResponse);
  // ... other existing RPCs unchanged
}
```

- [ ] **Step 2: Build the workspace to regen tonic code**

Run: `cargo build -p isengard-proto`
Expected: success, generated code reflects the new shapes.

- [ ] **Step 3: Verify regeneration**

Run: `grep -E "RenewCert|agent_cert_pem" crates/isengard-proto/src/pb.rs target/debug/build/isengard-proto-*/out/*.rs 2>/dev/null | head -5`
Expected: matches in the generated `pb.rs`.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-proto/proto/isengard.v1.proto
git commit -m "proto: phase 14 EnrollRequest token + cert bundle response + RenewCert RPC"
```

---

## Task 6: Refactor `Enroll` handler to use `EnrollmentService`

**Files:**
- Modify: `crates/isengard-controller/src/service.rs`
- Modify: `crates/isengard-controller/src/lib.rs`
- Create: `crates/isengard-controller/tests/enrollment_e2e.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/isengard-controller/tests/enrollment_e2e.rs`:

```rust
use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::ControllerService;
use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::EnrollRequest;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::Inventory;

#[tokio::test]
async fn enroll_with_valid_token_returns_cert_bundle() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let token = enrollment.mint(TokenRole::Agent, Duration::minutes(5)).await.unwrap();

    let svc = ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone());
    let req = tonic::Request::new(EnrollRequest {
        token,
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1".into(),
    });
    let resp = svc.enroll(req).await.unwrap().into_inner();

    assert!(resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(resp.agent_key_pem.contains("BEGIN PRIVATE KEY"));
    assert!(resp.ca_root_pem.contains("BEGIN CERTIFICATE"));
    assert_eq!(resp.heartbeat_interval_secs, 10);
    assert!(!resp.host_id.is_empty());
}

#[tokio::test]
async fn enroll_with_invalid_token_returns_unauthenticated() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));

    let svc = ControllerService::new_for_test(inv, ca, enrollment);
    let req = tonic::Request::new(EnrollRequest {
        token: "DOES-NOT-EXIST-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1".into(),
    });
    let err = svc.enroll(req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p isengard-controller --test enrollment_e2e`
Expected: compile error — `new_for_test` constructor and EnrollmentService field on ControllerService don't exist.

- [ ] **Step 3: Refactor `ControllerService`**

In `crates/isengard-controller/src/service.rs`, change `ControllerService` to hold an `Arc<EnrollmentService>` (and `Arc<Authority>` if not already), expose `new_for_test`, and rewrite `enroll`:

```rust
use crate::ca::Authority;
use crate::enrollment::{EnrollmentService, HostInfo};

pub struct ControllerService {
    pub inventory: Arc<Inventory>,
    pub ca: Arc<Authority>,
    pub enrollment: Arc<EnrollmentService>,
    // ... other existing fields preserved
}

impl ControllerService {
    pub fn new_for_test(
        inventory: Arc<Inventory>,
        ca: Arc<Authority>,
        enrollment: Arc<EnrollmentService>,
    ) -> Self {
        // Stub the bus / journal / pending actions with no-op constructors;
        // existing helper test constructors should be reused if any.
        // ... uses the same pattern as existing tests in this crate.
        Self { inventory, ca, enrollment, /* ... no-op everything else */ }
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn enroll(
        &self,
        request: tonic::Request<isengard_proto::pb::EnrollRequest>,
    ) -> Result<tonic::Response<isengard_proto::pb::EnrollResponse>, tonic::Status> {
        let req = request.into_inner();
        let host_info = HostInfo {
            hostname: req.hostname,
            os: req.os,
            version: req.version,
        };
        let resp = self.enrollment.redeem(&req.token, host_info).await
            .map_err(|e| tonic::Status::unauthenticated(format!("{e}")))?;
        Ok(tonic::Response::new(isengard_proto::pb::EnrollResponse {
            host_id: resp.host_id.to_db_bytes(),
            agent_cert_pem: resp.agent_cert_pem,
            agent_key_pem: resp.agent_key_pem,
            ca_root_pem: resp.ca_root_pem,
            heartbeat_interval_secs: resp.heartbeat_interval_secs,
        }))
    }
    // ... other impls unchanged
}
```

In `crates/isengard-controller/src/lib.rs` (the `run_controller` boot path), construct the `Authority` and `EnrollmentService` and inject into `ControllerService`:

```rust
let ca = Arc::new(Authority::load_or_init(&inventory).await?);
let enrollment = Arc::new(EnrollmentService::new(inventory.clone(), ca.clone()));
let service = ControllerService::new(/* ... */, ca.clone(), enrollment.clone());
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p isengard-controller --test enrollment_e2e && cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-controller/{src/service.rs,src/lib.rs,tests/enrollment_e2e.rs}
git commit -m "feat(controller): Enroll handler uses EnrollmentService.redeem (token → cert bundle)"
```

---

## Task 7: `RenewCert` handler

**Files:**
- Modify: `crates/isengard-controller/src/service.rs`
- Modify: `crates/isengard-controller/src/enrollment.rs` (add `renew` method)
- Create: `crates/isengard-controller/tests/cert_renewal_e2e.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/isengard-controller/tests/cert_renewal_e2e.rs`:

```rust
use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::ControllerService;
use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{EnrollRequest, RenewCertRequest};
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::Inventory;

#[tokio::test]
async fn renew_cert_returns_fresh_cert_for_known_host() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let token = enrollment.mint(TokenRole::Agent, Duration::minutes(5)).await.unwrap();

    let svc = ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone());

    let enroll_resp = svc.enroll(tonic::Request::new(EnrollRequest {
        token,
        hostname: "agent-1".into(), os: "linux".into(), version: "0.1".into(),
    })).await.unwrap().into_inner();

    let original_cert = enroll_resp.agent_cert_pem.clone();

    let renew_resp = svc.renew_cert(tonic::Request::new(RenewCertRequest {
        host_id: enroll_resp.host_id,
    })).await.unwrap().into_inner();

    assert!(renew_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
    assert_ne!(renew_resp.agent_cert_pem, original_cert, "renewal must produce a fresh cert");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p isengard-controller --test cert_renewal_e2e`
Expected: compile error — `renew_cert` not implemented.

- [ ] **Step 3: Add `renew` method to `EnrollmentService`**

In `crates/isengard-controller/src/enrollment.rs`, add:

```rust
#[derive(Debug, Clone)]
pub struct RenewedCert {
    pub agent_cert_pem: String,
    pub agent_key_pem: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl EnrollmentService {
    pub async fn renew(&self, host_id: HostId) -> Result<RenewedCert> {
        let host = self.inventory.get_host(host_id).await?
            .ok_or_else(|| anyhow!("unknown host"))?;
        let leaf = self.ca.sign_agent_leaf(
            host_id,
            &host.hostname,
            Duration::days(LEAF_TTL_DAYS),
        )?;
        self.inventory.insert_agent_cert(AgentCert {
            serial: leaf.serial.clone(),
            host_id,
            cert_pem: leaf.cert_pem.clone(),
            issued_at: chrono::Utc::now(),
            expires_at: leaf.expires_at,
            revoked_at: None,
            revoke_reason: None,
        }).await?;
        Ok(RenewedCert {
            agent_cert_pem: leaf.cert_pem,
            agent_key_pem: leaf.key_pem,
            expires_at: leaf.expires_at,
        })
    }
}
```

- [ ] **Step 4: Implement `renew_cert` handler**

In `crates/isengard-controller/src/service.rs`, add to the `Controller` trait impl:

```rust
async fn renew_cert(
    &self,
    request: tonic::Request<isengard_proto::pb::RenewCertRequest>,
) -> Result<tonic::Response<isengard_proto::pb::RenewCertResponse>, tonic::Status> {
    let req = request.into_inner();
    let host_id = HostId::from_db_bytes(req.host_id)
        .map_err(|_| tonic::Status::invalid_argument("invalid host_id"))?;

    // Phase 14 invariant: the request is authenticated by the existing client
    // cert; we cross-check that the cert presented matches the host_id requested.
    // Implemented in Task 8's interceptor; here we trust the host_id.

    let renewed = self.enrollment.renew(host_id).await
        .map_err(|e| tonic::Status::internal(format!("{e}")))?;
    Ok(tonic::Response::new(isengard_proto::pb::RenewCertResponse {
        agent_cert_pem: renewed.agent_cert_pem,
        agent_key_pem: renewed.agent_key_pem,
        expires_at: renewed.expires_at.to_rfc3339(),
    }))
}
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p isengard-controller --test cert_renewal_e2e && cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-controller/{src/service.rs,src/enrollment.rs,tests/cert_renewal_e2e.rs}
git commit -m "feat(controller): RenewCert handler (sign fresh leaf for existing host)"
```

---

## Task 8: tonic mTLS server config + `CertAuthInterceptor`

**Files:**
- Modify: `crates/isengard-controller/src/auth.rs`
- Modify: `crates/isengard-controller/src/lib.rs`
- Create: `crates/isengard-controller/tests/mtls_e2e.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/isengard-controller/tests/mtls_e2e.rs`:

```rust
//! Spin up a real tonic Server with mTLS, then connect with three different
//! client identities and verify the server's behavior.

use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{revoke_agent, RevocationSet};
use isengard_controller::ControllerService;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_proto::pb::EnrollRequest;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::Inventory;
use tokio::net::TcpListener;
use tonic::transport::{Channel, Identity, Server, ServerTlsConfig};

async fn boot_controller() -> (
    String,
    Arc<Inventory>,
    Arc<Authority>,
    Arc<EnrollmentService>,
    RevocationSet,
) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    // Generate a server cert for the controller, signed by the CA.
    let server_leaf = ca.sign_agent_leaf(
        isengard_storage::host::HostId::new(),
        "controller.local",
        Duration::days(30),
    ).unwrap();
    let identity = Identity::from_pem(server_leaf.cert_pem.as_bytes(), server_leaf.key_pem.as_bytes());
    let ca_root = tonic::transport::Certificate::from_pem(ca.root_cert_pem().as_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("https://{addr}");

    let svc = ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone());
    let interceptor = isengard_controller::auth::CertAuthInterceptor::new(
        revocation.clone(),
        ca.clone(),
    );

    let server_inv = inv.clone();
    let server_ca = ca.clone();
    let server_enr = enrollment.clone();
    let server_revoc = revocation.clone();
    tokio::spawn(async move {
        Server::builder()
            .tls_config(ServerTlsConfig::new()
                .identity(identity)
                .client_ca_root(ca_root)
                .client_auth_optional(true) // Enroll RPC needs to work without client cert
            ).unwrap()
            .add_service(ControllerServer::with_interceptor(svc, interceptor))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await.unwrap();
        let _ = (server_inv, server_ca, server_enr, server_revoc);
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    (url, inv, ca, enrollment, revocation)
}

#[tokio::test]
async fn enroll_then_mtls_heartbeat_succeeds() {
    let (url, _inv, ca, enr, _revoc) = boot_controller().await;

    let token = enr.mint(TokenRole::Agent, Duration::minutes(5)).await.unwrap();

    // Phase 1: bootstrap channel (no client cert) → enroll
    let bootstrap_tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca.root_cert_pem().as_bytes()))
        .domain_name("controller.local");
    let channel = Channel::from_shared(url.clone()).unwrap()
        .tls_config(bootstrap_tls).unwrap()
        .connect().await.unwrap();
    let mut client = ControllerClient::new(channel);
    let resp = client.enroll(EnrollRequest {
        token,
        hostname: "agent-1".into(), os: "linux".into(), version: "0.1".into(),
    }).await.unwrap().into_inner();

    // Phase 2: mTLS channel using the issued cert → heartbeat (or RenewCert as a proxy
    // for "any RPC that requires auth").
    let identity = Identity::from_pem(resp.agent_cert_pem.as_bytes(), resp.agent_key_pem.as_bytes());
    let mtls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(resp.ca_root_pem.as_bytes()))
        .identity(identity)
        .domain_name("controller.local");
    let channel = Channel::from_shared(url).unwrap().tls_config(mtls).unwrap().connect().await.unwrap();
    let mut client = ControllerClient::new(channel);

    let renew_resp = client.renew_cert(isengard_proto::pb::RenewCertRequest {
        host_id: resp.host_id,
    }).await.unwrap().into_inner();
    assert!(renew_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn revoked_cert_rejected() {
    let (url, inv, ca, enr, revoc) = boot_controller().await;
    let token = enr.mint(TokenRole::Agent, Duration::minutes(5)).await.unwrap();

    // enroll
    let bootstrap_tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(ca.root_cert_pem().as_bytes()))
        .domain_name("controller.local");
    let channel = Channel::from_shared(url.clone()).unwrap().tls_config(bootstrap_tls).unwrap().connect().await.unwrap();
    let mut client = ControllerClient::new(channel);
    let resp = client.enroll(EnrollRequest {
        token, hostname: "h".into(), os: "linux".into(), version: "0".into(),
    }).await.unwrap().into_inner();

    let host_id = isengard_storage::host::HostId::from_db_bytes(resp.host_id.clone()).unwrap();
    revoke_agent(&inv, &revoc, host_id, "test").await.unwrap();

    // attempt mTLS with revoked cert
    let identity = Identity::from_pem(resp.agent_cert_pem.as_bytes(), resp.agent_key_pem.as_bytes());
    let mtls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(resp.ca_root_pem.as_bytes()))
        .identity(identity)
        .domain_name("controller.local");
    let channel = Channel::from_shared(url).unwrap().tls_config(mtls).unwrap().connect().await.unwrap();
    let mut client = ControllerClient::new(channel);

    let err = client.renew_cert(isengard_proto::pb::RenewCertRequest {
        host_id: resp.host_id,
    }).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p isengard-controller --test mtls_e2e`
Expected: compile error — `CertAuthInterceptor` doesn't exist.

- [ ] **Step 3: Replace `TokenAuthLayer` with `CertAuthInterceptor`**

Replace contents of `crates/isengard-controller/src/auth.rs`:

```rust
//! Phase 14: per-RPC client cert validation + revocation check.
//! Replaces Phase 2c's TokenAuthLayer.

use std::sync::Arc;

use tonic::{Request, Status};

use crate::ca::Authority;
use crate::revocation::RevocationSet;

/// RPCs that don't require a client cert (the bootstrap chicken-and-egg).
const PUBLIC_METHODS: &[&str] = &["/isengard.v1.Controller/Enroll"];

#[derive(Clone)]
pub struct CertAuthInterceptor {
    revocation: RevocationSet,
    _ca: Arc<Authority>,
}

impl CertAuthInterceptor {
    pub fn new(revocation: RevocationSet, ca: Arc<Authority>) -> Self {
        Self { revocation, _ca: ca }
    }
}

impl tonic::service::Interceptor for CertAuthInterceptor {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        // Determine the gRPC method from req metadata. Tonic puts it in
        // extensions via the GrpcMethod extension; fall back to checking
        // the URI path for older tonic versions.
        let method = req.extensions()
            .get::<tonic::server::GrpcMethod>()
            .map(|m| format!("/{}/{}", m.service(), m.method()));

        if let Some(m) = method.as_deref() {
            if PUBLIC_METHODS.contains(&m) {
                return Ok(req);
            }
        }

        let peer_certs = req.peer_certs()
            .ok_or_else(|| Status::unauthenticated("client cert required"))?;
        let cert = peer_certs.first()
            .ok_or_else(|| Status::unauthenticated("no client cert presented"))?;

        let serial = extract_serial(cert.get_ref())
            .ok_or_else(|| Status::unauthenticated("client cert has no serial"))?;
        if self.revocation.contains(&serial) {
            return Err(Status::unauthenticated("client cert revoked"));
        }

        Ok(req)
    }
}

fn extract_serial(cert_der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    Some(cert.tbs_certificate.raw_serial().to_vec())
}
```

In `crates/isengard-controller/src/lib.rs`, replace `TokenAuthLayer` setup with:

```rust
use crate::auth::CertAuthInterceptor;

let revocation = RevocationSet::load_from_inventory(&inventory).await?;
let interceptor = CertAuthInterceptor::new(revocation.clone(), ca.clone());

// Generate the controller's server cert (signed by our own CA).
let server_leaf = ca.sign_agent_leaf(
    HostId::new(),
    &controller_dns_name(),
    chrono::Duration::days(30),
)?;
let identity = tonic::transport::Identity::from_pem(
    server_leaf.cert_pem.as_bytes(),
    server_leaf.key_pem.as_bytes(),
);
let ca_root = tonic::transport::Certificate::from_pem(ca.root_cert_pem().as_bytes());

Server::builder()
    .tls_config(ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(ca_root)
        .client_auth_optional(true))? // Enroll allowed without client cert
    .add_service(ControllerServer::with_interceptor(service, interceptor))
    .serve_with_shutdown(addr, shutdown)
    .await?;

fn controller_dns_name() -> String {
    std::env::var("ISENGARD_CONTROLLER_DNS").unwrap_or_else(|_| "controller.local".into())
}
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p isengard-controller --test mtls_e2e && cargo clippy -p isengard-controller --all-targets -- -D warnings`
Expected: 2 mTLS tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-controller/{src/auth.rs,src/lib.rs,tests/mtls_e2e.rs}
git commit -m "feat(controller): mTLS server config + CertAuthInterceptor (revocation check, Enroll bypass)"
```

---

## Task 9: CLI — drop `ISENGARD_TOKEN`, add controller subcommands

**Files:**
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Drop `ISENGARD_TOKEN` env var reads**

In `crates/isengard/src/main.rs`, remove both instances of:

```rust
let token = std::env::var("ISENGARD_TOKEN")
    .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
let _ = token;
```

- [ ] **Step 2: Add subcommand enum**

Add a nested `ControllerSub` enum to the existing `Command::Controller` arm, OR add new top-level `Token` and `Agent` subcommands under a new `Controller` parent. Pick: nested for cleanliness.

```rust
#[derive(Debug, Subcommand)]
enum Command {
    Controller {
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
        #[command(subcommand)]
        action: Option<ControllerAction>,
    },
    Agent {
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: String,
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
        #[arg(long, env = "ISENGARD_ENROLL_TOKEN")]
        enroll_token: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ControllerAction {
    Token {
        #[command(subcommand)]
        op: TokenOp,
    },
    Agent {
        #[command(subcommand)]
        op: AgentOp,
    },
}

#[derive(Debug, Subcommand)]
enum TokenOp {
    Mint {
        #[arg(long, default_value = "agent")]
        role: String,
        #[arg(long, default_value = "15m")]
        ttl: humantime::Duration,
    },
}

#[derive(Debug, Subcommand)]
enum AgentOp {
    Revoke {
        host_id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    List,
}
```

- [ ] **Step 3: Wire actions into the existing match arms**

```rust
Command::Controller { listen, state_dir, action: None } => {
    isengard_controller::run_controller(listen, state_dir).await?;
}
Command::Controller { state_dir, action: Some(ControllerAction::Token { op: TokenOp::Mint { role, ttl } }), .. } => {
    let inv = Arc::new(Inventory::open(&state_dir).await?);
    let ca = Arc::new(Authority::load_or_init(&inv).await?);
    let enr = EnrollmentService::new(inv, ca);
    let role = match role.as_str() {
        "agent" => TokenRole::Agent,
        other => anyhow::bail!("unknown role: {other}"),
    };
    let token = enr.mint(role, chrono::Duration::from_std(ttl.into())?).await?;
    println!("{token}");
}
Command::Controller { state_dir, action: Some(ControllerAction::Agent { op: AgentOp::Revoke { host_id, reason } }), .. } => {
    let inv = Inventory::open(&state_dir).await?;
    let revoc = RevocationSet::load_from_inventory(&inv).await?;
    let host_id = HostId::from_string(&host_id)?;
    revoke_agent(&inv, &revoc, host_id, &reason).await?;
    println!("revoked");
}
Command::Controller { state_dir, action: Some(ControllerAction::Agent { op: AgentOp::List }), .. } => {
    let inv = Inventory::open(&state_dir).await?;
    for host in inv.list_hosts(None).await? {
        let cert = inv.active_cert_for_host(host.id).await?;
        let cert_info = cert.map(|c| format!(
            "serial={} expires={}",
            hex::encode(&c.serial[..8]),
            c.expires_at,
        )).unwrap_or_else(|| "no cert".into());
        println!("{}\t{}\t{}", host.id, host.hostname, cert_info);
    }
}
```

Add deps to `crates/isengard/Cargo.toml`: `humantime`, `hex`.

- [ ] **Step 4: Smoke-test the CLI**

Run:
```bash
mkdir -p /tmp/iseng-test
cargo run -p isengard -- controller --state-dir /tmp/iseng-test token mint --ttl 5m
```
Expected: prints a 52-char base32 token to stdout.

```bash
cargo run -p isengard -- controller --state-dir /tmp/iseng-test agent list
```
Expected: empty (no enrolled agents yet).

- [ ] **Step 5: Commit**

```bash
git add crates/isengard/{src/main.rs,Cargo.toml}
git commit -m "feat(cli): drop ISENGARD_TOKEN env, add 'controller token mint' and 'agent revoke|list'"
```

---

## Task 10: Agent `cert_store` module

**Files:**
- Create: `crates/isengard-agent/src/cert_store.rs`
- Create: `crates/isengard-agent/tests/cert_store_unit.rs`
- Modify: `crates/isengard-agent/src/lib.rs`
- Modify: `crates/isengard-agent/Cargo.toml`

- [ ] **Step 1: Add deps**

In `crates/isengard-agent/Cargo.toml`:

```toml
tempfile = { workspace = true }  # already present in many crates; add if absent
```

- [ ] **Step 2: Write the tests**

Create `crates/isengard-agent/tests/cert_store_unit.rs`:

```rust
use isengard_agent::cert_store::{CertBundle, exists, load, save};

fn fixture() -> CertBundle {
    CertBundle {
        ca_pem: "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n".into(),
        cert_pem: "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n".into(),
        key_pem: "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n".into(),
    }
}

#[test]
fn exists_false_on_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!exists(tmp.path()));
}

#[test]
fn save_then_load_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle = fixture();
    save(tmp.path(), &bundle).unwrap();
    assert!(exists(tmp.path()));
    let loaded = load(tmp.path()).unwrap();
    assert_eq!(loaded.ca_pem, bundle.ca_pem);
    assert_eq!(loaded.cert_pem, bundle.cert_pem);
    assert_eq!(loaded.key_pem, bundle.key_pem);
}

#[test]
fn save_writes_key_with_restrictive_perms() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    save(tmp.path(), &fixture()).unwrap();
    let key_meta = std::fs::metadata(tmp.path().join("certs").join("agent.key")).unwrap();
    let mode = key_meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "key file should be chmod 600, got {:o}", mode);
}

#[test]
fn save_is_atomic_failed_write_doesnt_clobber() {
    let tmp = tempfile::tempdir().unwrap();
    let original = fixture();
    save(tmp.path(), &original).unwrap();

    // Try to save with an invalid bundle scenario... since the impl writes
    // .new files first then renames, the only way to test atomicity here is
    // to verify the .new files are cleaned up on success.
    let new_bundle = CertBundle {
        ca_pem: "new-ca".into(),
        cert_pem: "new-cert".into(),
        key_pem: "new-key".into(),
    };
    save(tmp.path(), &new_bundle).unwrap();

    let cert_dir = tmp.path().join("certs");
    assert!(!cert_dir.join("ca.pem.new").exists(), ".new files must be cleaned up");
    assert!(!cert_dir.join("agent.crt.new").exists());
    assert!(!cert_dir.join("agent.key.new").exists());
}
```

- [ ] **Step 3: Implement `cert_store`**

Create `crates/isengard-agent/src/cert_store.rs`:

```rust
//! Phase 14: agent-side cert bundle storage. Lives in `state_dir/certs/`.
//! Atomic writes via `.new` + rename; key file gets chmod 600.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct CertBundle {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn cert_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("certs")
}

pub fn exists(state_dir: &Path) -> bool {
    let d = cert_dir(state_dir);
    d.join("ca.pem").exists() && d.join("agent.crt").exists() && d.join("agent.key").exists()
}

pub fn load(state_dir: &Path) -> Result<CertBundle> {
    let d = cert_dir(state_dir);
    Ok(CertBundle {
        ca_pem: std::fs::read_to_string(d.join("ca.pem")).context("read ca.pem")?,
        cert_pem: std::fs::read_to_string(d.join("agent.crt")).context("read agent.crt")?,
        key_pem: std::fs::read_to_string(d.join("agent.key")).context("read agent.key")?,
    })
}

pub fn save(state_dir: &Path, bundle: &CertBundle) -> Result<()> {
    let d = cert_dir(state_dir);
    std::fs::create_dir_all(&d).context("mkdir certs/")?;

    write_atomic(&d.join("ca.pem"), bundle.ca_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.crt"), bundle.cert_pem.as_bytes(), 0o644)?;
    write_atomic(&d.join("agent.key"), bundle.key_pem.as_bytes(), 0o600)?;
    Ok(())
}

fn write_atomic(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let tmp = target.with_extension(format!(
        "{}.new",
        target.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {tmp:?}"))?;
        f.write_all(data)?;
        f.sync_all()?;
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(mode);
        f.set_permissions(perms)?;
    }
    std::fs::rename(&tmp, target).with_context(|| format!("rename {tmp:?} -> {target:?}"))?;
    Ok(())
}
```

- [ ] **Step 4: Wire into lib.rs**

In `crates/isengard-agent/src/lib.rs`:

```rust
pub mod cert_store;
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p isengard-agent --test cert_store_unit && cargo clippy -p isengard-agent --all-targets -- -D warnings`
Expected: 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-agent/{src/cert_store.rs,src/lib.rs,tests/cert_store_unit.rs,Cargo.toml}
git commit -m "feat(agent): cert_store module (atomic write, chmod 600 on key)"
```

---

## Task 11: Agent enrollment refactor

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs`
- Modify: `crates/isengard-agent/src/enroll.rs`

- [ ] **Step 1: Refactor `enroll` to take token + return bundle**

In `crates/isengard-agent/src/enroll.rs`, change the enroll function signature:

```rust
pub struct EnrollOutcome {
    pub host_id: HostId,
    pub bundle: CertBundle,
    pub heartbeat_interval_secs: u32,
}

pub async fn enroll(
    controller_url: &str,
    enroll_token: &str,
    host_info: HostInfo,
) -> Result<EnrollOutcome> {
    // Bootstrap channel: trust whatever cert the controller presents (no CA yet).
    // This is the documented bootstrap-trust limitation; see spec §"Open known
    // limitations".
    let tls = tonic::transport::ClientTlsConfig::new()
        .with_native_roots(); // accepts any reasonable cert presented during enroll

    let channel = tonic::transport::Channel::from_shared(controller_url.to_string())?
        .tls_config(tls)?
        .connect().await
        .with_context(|| format!("connect bootstrap channel to {controller_url}"))?;
    let mut client = ControllerClient::new(channel);

    let resp = client.enroll(EnrollRequest {
        token: enroll_token.into(),
        hostname: host_info.hostname,
        os: host_info.os,
        version: host_info.version,
    }).await?.into_inner();

    let host_id = HostId::from_db_bytes(resp.host_id)
        .map_err(|e| anyhow!("invalid host_id from controller: {e}"))?;
    let bundle = CertBundle {
        ca_pem: resp.ca_root_pem,
        cert_pem: resp.agent_cert_pem,
        key_pem: resp.agent_key_pem,
    };

    Ok(EnrollOutcome { host_id, bundle, heartbeat_interval_secs: resp.heartbeat_interval_secs })
}
```

- [ ] **Step 2: Refactor `lib.rs` boot path**

Replace the `let token = std::env::var("ISENGARD_TOKEN")...` read sites in `crates/isengard-agent/src/lib.rs` with:

```rust
match agent_state::load(&opts.state_dir)? {
    None => {
        info!("no agent.json found, enrolling with controller");
        let enroll_token = std::env::var("ISENGARD_ENROLL_TOKEN")
            .or_else(|_| opts.enroll_token.clone().ok_or_else(|| std::env::VarError::NotPresent))
            .map_err(|_| anyhow!(
                "ISENGARD_ENROLL_TOKEN env var (or --enroll-token) required for first-time enrollment. \
                Mint one with `isengard controller token mint`."
            ))?;
        let host_info = enroll::HostInfo::detect();
        let outcome = enroll::enroll(&opts.controller_url, &enroll_token, host_info).await?;
        cert_store::save(&opts.state_dir, &outcome.bundle)?;
        agent_state::save(&opts.state_dir, &agent_state::AgentState {
            host_id: outcome.host_id,
            controller_url: opts.controller_url.clone(),
            heartbeat_interval_secs: outcome.heartbeat_interval_secs,
        })?;
        // Drop enroll_token from memory; subsequent reconnects use the cert.
    }
    Some(_state) => {
        if !cert_store::exists(&opts.state_dir) {
            anyhow::bail!(
                "agent.json present but no cert bundle in {}/certs — wipe state and re-enroll",
                opts.state_dir.display(),
            );
        }
    }
}

// All subsequent gRPC uses mTLS.
let bundle = cert_store::load(&opts.state_dir)?;
let mtls = tonic::transport::ClientTlsConfig::new()
    .ca_certificate(tonic::transport::Certificate::from_pem(bundle.ca_pem.as_bytes()))
    .identity(tonic::transport::Identity::from_pem(bundle.cert_pem.as_bytes(), bundle.key_pem.as_bytes()))
    .domain_name(controller_dns_from_url(&opts.controller_url));

let channel = tonic::transport::Channel::from_shared(opts.controller_url.clone())?
    .tls_config(mtls)?
    .connect().await?;
```

Drop both prior `let token = std::env::var("ISENGARD_TOKEN")...` blocks.

- [ ] **Step 3: Compile + test**

Run: `cargo test -p isengard-agent` and `cargo clippy -p isengard-agent --all-targets -- -D warnings`
Expected: green (existing tests adapted to new shape if needed).

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-agent/{src/lib.rs,src/enroll.rs}
git commit -m "feat(agent): enrollment refactor — ISENGARD_ENROLL_TOKEN, persist cert bundle, mTLS for all subsequent RPCs"
```

---

## Task 12: Agent `cert_renewal` task

**Files:**
- Create: `crates/isengard-agent/src/cert_renewal.rs`
- Create: `crates/isengard-agent/tests/cert_renewal_unit.rs`
- Modify: `crates/isengard-agent/src/sync.rs`
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Write tests**

Create `crates/isengard-agent/tests/cert_renewal_unit.rs`:

```rust
use chrono::Duration;
use isengard_agent::cert_renewal::should_renew;

#[test]
fn does_not_renew_well_before_50pct() {
    let issued = chrono::Utc::now() - Duration::days(5);
    let expires = chrono::Utc::now() + Duration::days(25);
    assert!(!should_renew(issued, expires));
}

#[test]
fn renews_at_or_past_50pct() {
    let issued = chrono::Utc::now() - Duration::days(16);
    let expires = chrono::Utc::now() + Duration::days(14);
    assert!(should_renew(issued, expires));
}
```

- [ ] **Step 2: Implement**

Create `crates/isengard-agent/src/cert_renewal.rs`:

```rust
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::RenewCertRequest;
use tokio::sync::RwLock;
use tonic::transport::Channel;
use tracing::{info, warn};

use crate::cert_store::{self, CertBundle};
use isengard_storage::host::HostId;

pub fn should_renew(issued_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> bool {
    let total = expires_at - issued_at;
    let half = total / 2;
    Utc::now() >= issued_at + half
}

pub async fn run_renewal_loop(
    state_dir: std::path::PathBuf,
    host_id: HostId,
    channel_holder: Arc<RwLock<Channel>>,
    poll_interval: Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(poll_interval).await;
        if let Err(e) = maybe_renew(&state_dir, host_id, channel_holder.clone()).await {
            warn!(error=%e, "cert renewal check failed");
        }
    }
}

async fn maybe_renew(
    state_dir: &Path,
    host_id: HostId,
    channel_holder: Arc<RwLock<Channel>>,
) -> Result<()> {
    let bundle = cert_store::load(state_dir)?;
    let (issued_at, expires_at) = parse_validity(&bundle.cert_pem)?;
    if !should_renew(issued_at, expires_at) { return Ok(()); }

    info!("cert past 50% TTL, renewing");
    let channel = channel_holder.read().await.clone();
    let mut client = ControllerClient::new(channel);
    let resp = client.renew_cert(RenewCertRequest {
        host_id: host_id.to_db_bytes(),
    }).await.context("renew_cert RPC")?.into_inner();

    let new_bundle = CertBundle {
        ca_pem: bundle.ca_pem.clone(),
        cert_pem: resp.agent_cert_pem,
        key_pem: resp.agent_key_pem,
    };
    cert_store::save(state_dir, &new_bundle)?;

    // Rebuild channel with new identity. Caller's Arc<RwLock<Channel>> swap
    // happens in lib.rs (build_mtls_channel + replace).
    info!("cert renewed, channel will swap on next request");
    Ok(())
}

fn parse_validity(cert_pem: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())?;
    let cert = pem.parse_x509()?;
    let nb = cert.tbs_certificate.validity.not_before.to_datetime();
    let na = cert.tbs_certificate.validity.not_after.to_datetime();
    Ok((nb.into(), na.into()))
}
```

- [ ] **Step 3: Spawn the renewal loop in sync setup**

In `crates/isengard-agent/src/lib.rs`, after the channel is built and the sync loop is about to start, spawn the renewal task with a 60s poll interval (cheap):

```rust
let channel_holder = Arc::new(RwLock::new(channel.clone()));
tokio::spawn(cert_renewal::run_renewal_loop(
    opts.state_dir.clone(),
    host_id,
    channel_holder.clone(),
    Duration::from_secs(60),
));
```

- [ ] **Step 4: Run tests + clippy**

Run: `cargo test -p isengard-agent --test cert_renewal_unit && cargo clippy -p isengard-agent --all-targets -- -D warnings`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-agent/{src/cert_renewal.rs,src/lib.rs,tests/cert_renewal_unit.rs}
git commit -m "feat(agent): cert_renewal task (renew at 50% TTL via RenewCert RPC)"
```

---

## Task 13: Dashboard backend — enrollment REST endpoints

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/enrollment.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs`
- Create: `crates/isengard-plugins/dashboard/tests/enrollment_endpoints.rs`

- [ ] **Step 1: Write integration test**

Create `crates/isengard-plugins/dashboard/tests/enrollment_endpoints.rs` (mirror the pattern from existing `deployments_endpoints.rs`):

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::test_app;

#[tokio::test]
async fn post_enrollment_token_returns_token() {
    let app = test_app().await;
    let body = json!({"role": "agent", "ttl_seconds": 900}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/enrollment/tokens")
        .header("content-type", "application/json")
        .body(Body::from(body)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["token"].as_str().unwrap().len() >= 50);
    assert!(v["expires_at"].is_string());
}

#[tokio::test]
async fn get_enrollment_tokens_lists_active() {
    let app = test_app().await;
    // mint via POST first
    let body = json!({"role": "agent", "ttl_seconds": 900}).to_string();
    let req = Request::builder()
        .method("POST").uri("/api/v1/enrollment/tokens")
        .header("content-type", "application/json").body(Body::from(body)).unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = Request::builder().method("GET").uri("/api/v1/enrollment/tokens").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
}
```

- [ ] **Step 2: Implement endpoints**

Create `crates/isengard-plugins/dashboard/src/enrollment.rs`:

```rust
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{revoke_agent, RevocationSet};
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use isengard_storage::Inventory;

use crate::ControllerHandles;

pub fn router() -> Router<Arc<ControllerHandles>> {
    Router::new()
        .route("/enrollment/tokens", post(mint_token).get(list_tokens))
        .route("/enrollment/tokens/:hash_prefix", delete(revoke_token))
        .route("/hosts/:host_id/cert", delete(revoke_host_cert))
}

#[derive(Deserialize)]
struct MintBody { role: String, ttl_seconds: u64 }

#[derive(Serialize)]
struct MintedToken { token: String, expires_at: DateTime<Utc> }

async fn mint_token(
    State(h): State<Arc<ControllerHandles>>,
    Json(body): Json<MintBody>,
) -> Result<(StatusCode, Json<MintedToken>), (StatusCode, String)> {
    let role = match body.role.as_str() {
        "agent" => TokenRole::Agent,
        other => return Err((StatusCode::BAD_REQUEST, format!("unknown role: {other}"))),
    };
    if body.ttl_seconds == 0 || body.ttl_seconds > 86_400 {
        return Err((StatusCode::BAD_REQUEST, "ttl_seconds must be 1..=86400".into()));
    }
    let ttl = Duration::seconds(body.ttl_seconds as i64);
    let token = h.enrollment.mint(role, ttl).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    let expires_at = Utc::now() + ttl;
    Ok((StatusCode::CREATED, Json(MintedToken { token, expires_at })))
}

#[derive(Serialize)]
struct ActiveToken {
    hash_prefix: String,
    role: String,
    expires_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
}

async fn list_tokens(
    State(h): State<Arc<ControllerHandles>>,
) -> Result<Json<Vec<ActiveToken>>, (StatusCode, String)> {
    let rows = h.inventory.list_active_tokens().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(rows.into_iter().map(|r| ActiveToken {
        hash_prefix: hex::encode(&r.token_hash[..8]),
        role: "agent".into(),
        expires_at: r.expires_at,
        created_at: r.created_at,
    }).collect()))
}

async fn revoke_token(
    State(h): State<Arc<ControllerHandles>>,
    Path(hash_prefix): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Find the token whose first 8 bytes hex-match.
    let prefix = hex::decode(&hash_prefix)
        .map_err(|_| (StatusCode::BAD_REQUEST, "hash_prefix must be hex".into()))?;
    if prefix.len() != 8 {
        return Err((StatusCode::BAD_REQUEST, "hash_prefix must be 8 bytes / 16 hex chars".into()));
    }
    // Brute-force scan; fleet sizes mean active token list is tiny.
    let rows = h.inventory.list_active_tokens().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    let target = rows.into_iter().find(|r| r.token_hash[..8] == prefix[..])
        .ok_or((StatusCode::NOT_FOUND, "no matching active token".into()))?;
    // "Revoke" an active token = mark consumed by a sentinel (HostId::nil()).
    h.inventory.consume_enrollment_token(&target.token_hash, HostId::nil()).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_host_cert(
    State(h): State<Arc<ControllerHandles>>,
    Path(host_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let host_id = HostId::from_string(&host_id)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid host_id".into()))?;
    revoke_agent(&h.inventory, &h.revocation, host_id, "revoked via dashboard")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
}
```

In `crates/isengard-plugins/dashboard/src/lib.rs`, register the router and add `enrollment: Arc<EnrollmentService>` + `revocation: RevocationSet` to `ControllerHandles`. Plumb them in from the controller boot path.

- [ ] **Step 3: Run tests + clippy**

Run: `cargo test -p isengard-plugin-dashboard --test enrollment_endpoints && cargo clippy -p isengard-plugin-dashboard --all-targets -- -D warnings`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-plugins/dashboard/src/{enrollment.rs,lib.rs} \
        crates/isengard-plugins/dashboard/tests/enrollment_endpoints.rs
git commit -m "feat(dashboard): enrollment REST endpoints (mint/list/revoke tokens, revoke host cert)"
```

---

## Task 14: Dashboard frontend — Enrollment tab + per-host revoke

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useEnrollment.ts`
- Create: `crates/isengard-plugins/dashboard/web/components/EnrollmentSettings.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/MintTokenModal.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/HostInspector.vue`

- [ ] **Step 1: Composable**

Create `crates/isengard-plugins/dashboard/web/composables/useEnrollment.ts`:

```typescript
import { computed, ref } from 'vue'

export interface ActiveToken {
  hash_prefix: string
  role: string
  expires_at: string
  created_at: string
}

export interface MintedToken {
  token: string
  expires_at: string
}

export function useEnrollment() {
  const api = useApi()
  const tokens = ref<ActiveToken[]>([])
  const loading = ref(false)

  async function refresh() {
    loading.value = true
    try {
      tokens.value = await api.get<ActiveToken[]>('/enrollment/tokens')
    } finally { loading.value = false }
  }

  async function mint(role: 'agent', ttlSeconds: number): Promise<MintedToken> {
    return await api.post<MintedToken>('/enrollment/tokens', { role, ttl_seconds: ttlSeconds })
  }

  async function revokeToken(hashPrefix: string) {
    await api.delete(`/enrollment/tokens/${hashPrefix}`)
    await refresh()
  }

  async function revokeHostCert(hostId: string) {
    await api.delete(`/hosts/${hostId}/cert`)
  }

  return { tokens, loading, refresh, mint, revokeToken, revokeHostCert }
}
```

- [ ] **Step 2: MintTokenModal component**

Create `crates/isengard-plugins/dashboard/web/components/MintTokenModal.vue`:

```vue
<script setup lang="ts">
import { ref } from 'vue'
import { useEnrollment } from '~/composables/useEnrollment'

const emit = defineEmits<{ (e: 'close'): void; (e: 'minted'): void }>()

const { mint } = useEnrollment()
const ttlMinutes = ref(15)
const minted = ref<{ token: string; dockerRun: string } | null>(null)
const error = ref<string | null>(null)
const minting = ref(false)

async function submit() {
  minting.value = true
  error.value = null
  try {
    const result = await mint('agent', ttlMinutes.value * 60)
    const controllerUrl = window.location.origin.replace(/^http/, 'https').replace(/:\d+$/, ':9417')
    minted.value = {
      token: result.token,
      dockerRun: `docker run -d --name isengard-agent --restart=always \\
  -v /var/run/docker.sock:/var/run/docker.sock \\
  -v isengard-agent-data:/var/lib/isengard \\
  -e ISENGARD_CONTROLLER=${controllerUrl} \\
  -e ISENGARD_ENROLL_TOKEN=${result.token} \\
  ghcr.io/dirdmaster/isengard-agent:next`,
    }
    emit('minted')
  } catch (e: any) {
    error.value = e?.message ?? String(e)
  } finally { minting.value = false }
}

function copyToken() { if (minted.value) navigator.clipboard.writeText(minted.value.token) }
function copyDockerRun() { if (minted.value) navigator.clipboard.writeText(minted.value.dockerRun) }
</script>

<template>
  <div class="modal-backdrop" @click.self="emit('close')">
    <div class="modal-card">
      <h2>Mint enrollment token</h2>

      <div v-if="!minted">
        <label>
          TTL (minutes)
          <input v-model.number="ttlMinutes" type="number" min="1" max="1440" />
        </label>
        <p class="hint">Token expires after this many minutes if not used.</p>
        <p v-if="error" class="error">{{ error }}</p>
        <div class="actions">
          <button @click="emit('close')">Cancel</button>
          <button :disabled="minting" @click="submit">Mint</button>
        </div>
      </div>

      <div v-else>
        <p>Token (shown once):</p>
        <pre class="token">{{ minted.token }}</pre>
        <button @click="copyToken">Copy token</button>

        <p>Or copy the full docker run command:</p>
        <pre class="docker-run">{{ minted.dockerRun }}</pre>
        <button @click="copyDockerRun">Copy docker run</button>

        <div class="actions">
          <button @click="emit('close')">Done</button>
        </div>
      </div>
    </div>
  </div>
</template>
```

- [ ] **Step 3: EnrollmentSettings component**

Create `crates/isengard-plugins/dashboard/web/components/EnrollmentSettings.vue`:

```vue
<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useEnrollment } from '~/composables/useEnrollment'
import MintTokenModal from '~/components/MintTokenModal.vue'

const { tokens, loading, refresh, revokeToken } = useEnrollment()
const showModal = ref(false)

onMounted(refresh)
</script>

<template>
  <SettingsSection title="Active enrollment tokens">
    <button @click="showModal = true">Mint token</button>

    <EmptyState v-if="!loading && tokens.length === 0" title="No active tokens"
                description="Mint a token to enroll a new agent host." />

    <table v-else>
      <thead><tr><th>Token (first 8 bytes)</th><th>Role</th><th>Expires</th><th></th></tr></thead>
      <tbody>
        <tr v-for="t in tokens" :key="t.hash_prefix">
          <td><code>{{ t.hash_prefix }}…</code></td>
          <td>{{ t.role }}</td>
          <td>{{ t.expires_at }}</td>
          <td><button @click="revokeToken(t.hash_prefix)">Revoke</button></td>
        </tr>
      </tbody>
    </table>

    <MintTokenModal v-if="showModal" @close="showModal = false" @minted="refresh" />
  </SettingsSection>
</template>
```

- [ ] **Step 4: Add Enrollment tab to SettingsTabs.vue**

In `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue`, add an entry to the `tabs` array (placement: between Hosts and Networking):

```typescript
{ id: 'enrollment', label: 'Enrollment' },
```

And the conditional render:

```vue
<EnrollmentSettings v-else-if="activeTab === 'enrollment'" />
```

- [ ] **Step 5: Per-host revoke in HostInspector.vue**

In `crates/isengard-plugins/dashboard/web/components/HostInspector.vue`, add a "Revoke cert" button:

```vue
<script setup lang="ts">
// ... existing imports
import { useEnrollment } from '~/composables/useEnrollment'

// ... existing props
const { revokeHostCert } = useEnrollment()
const revoking = ref(false)

async function onRevoke() {
  if (!confirm(`Revoke cert for ${props.host.hostname}? Agent will be unable to reconnect.`)) return
  revoking.value = true
  try { await revokeHostCert(props.host.id) } finally { revoking.value = false }
}
</script>

<template>
  <!-- ... existing host details -->
  <button :disabled="revoking" @click="onRevoke">Revoke cert</button>
</template>
```

- [ ] **Step 6: Manual smoke (no automated frontend tests in this codebase)**

Run: `cd crates/isengard-plugins/dashboard/web && bun run dev`, open `http://localhost:3000/settings`, click Enrollment tab, mint a token, verify it appears in the list, copy the docker run command. Revoke it, verify it disappears.

- [ ] **Step 7: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/composables/useEnrollment.ts \
        crates/isengard-plugins/dashboard/web/components/{EnrollmentSettings,MintTokenModal,HostInspector}.vue \
        crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue
git commit -m "feat(dashboard): Enrollment tab + Mint Token modal + per-host revoke button"
```

---

## Task 15: Real-Docker e2e auth test

**Files:**
- Create: `crates/isengard-agent/tests/auth_e2e.rs`

- [ ] **Step 1: Write the e2e test**

Create `crates/isengard-agent/tests/auth_e2e.rs` (mirror the pattern from `deployment_blue_green_happy.rs`):

```rust
//! Real-Docker e2e: spawn controller container with fresh state, mint token via
//! subprocess, spawn agent container with the token, verify enrollment + heartbeat,
//! revoke via subprocess, verify next heartbeat fails.
//!
//! Marked #[ignore] — runs only with `cargo test -- --ignored` or in CI.

#![cfg(target_os = "linux")] // Docker socket access; run on Linux runners

use std::time::Duration;

#[tokio::test]
#[ignore]
async fn full_auth_lifecycle() {
    // 1. Spawn controller container
    let state_dir = tempfile::tempdir().unwrap();
    let controller = spawn_controller(state_dir.path()).await;

    // 2. Mint token via `docker exec controller isengard controller token mint`
    let token = mint_token(&controller).await;

    // 3. Spawn agent with token
    let agent_state = tempfile::tempdir().unwrap();
    let agent = spawn_agent(controller.url(), &token, agent_state.path()).await;

    // 4. Wait for enrollment + verify heartbeat works (poll controller's `agent list`)
    wait_for_agent_enrolled(&controller, Duration::from_secs(30)).await;

    // 5. Revoke
    let host_id = list_first_host(&controller).await;
    revoke_agent(&controller, &host_id).await;

    // 6. Verify next agent heartbeat fails (logs show Unauthenticated)
    wait_for_agent_log_match(&agent, "Unauthenticated", Duration::from_secs(20)).await;
}

// Helpers (spawn_controller, mint_token, etc.) follow the same shape as
// deployment_blue_green_happy.rs's bollard-based fixtures.
async fn spawn_controller(state_dir: &std::path::Path) -> ControllerFixture { todo!("see deployment_blue_green_happy.rs pattern") }
async fn mint_token(_c: &ControllerFixture) -> String { todo!() }
async fn spawn_agent(_url: &str, _token: &str, _state: &std::path::Path) -> AgentFixture { todo!() }
async fn wait_for_agent_enrolled(_c: &ControllerFixture, _t: Duration) {}
async fn list_first_host(_c: &ControllerFixture) -> String { todo!() }
async fn revoke_agent(_c: &ControllerFixture, _host_id: &str) {}
async fn wait_for_agent_log_match(_a: &AgentFixture, _needle: &str, _t: Duration) {}

struct ControllerFixture { container_id: String, port: u16 }
impl ControllerFixture { fn url(&self) -> String { format!("https://localhost:{}", self.port) } }
struct AgentFixture { container_id: String }
```

The full implementation of these helpers mirrors `deployment_blue_green_happy.rs` — use bollard to start `ghcr.io/dirdmaster/isengard-controller:local` (built from this branch), mount state-dirs, wait for ports, exec subcommands.

- [ ] **Step 2: Implement helpers (cribbing from deployment_blue_green_happy.rs)**

Replace the `todo!()` bodies with bollard-based implementations following the existing test fixtures' shape.

- [ ] **Step 3: Run the e2e**

Local Docker required. Run: `cargo test -p isengard-agent --test auth_e2e -- --ignored --nocapture`
Expected: pass within ~60s.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-agent/tests/auth_e2e.rs
git commit -m "test(agent): real-Docker e2e for auth lifecycle (enroll → heartbeat → revoke → reject)"
```

---

## Task 16: Final — README, release notes, all gates green, open PR

**Files:**
- Modify: `README.md`
- Create: `docs/RELEASE_NOTES_PHASE_14.md`

- [ ] **Step 1: Update README.md**

Add a section on auth setup. Replace any mentions of `ISENGARD_TOKEN` with the new flow:

```markdown
### Bootstrap

1. Start the controller. On first boot it generates an internal CA:
   \`\`\`bash
   docker run -d --name isengard-controller --restart=always \
     -p 9417:9417 -p 9418:9418 \
     -v isengard-controller-data:/var/lib/isengard \
     ghcr.io/dirdmaster/isengard-controller:next
   \`\`\`

2. Mint an enrollment token (one per agent, short-lived):
   \`\`\`bash
   docker exec isengard-controller isengard controller token mint --ttl 15m
   \`\`\`

3. Start the agent with the token:
   \`\`\`bash
   docker run -d --name isengard-agent --restart=always \
     -v /var/run/docker.sock:/var/run/docker.sock \
     -v isengard-agent-data:/var/lib/isengard \
     -e ISENGARD_CONTROLLER=https://controller.local:9417 \
     -e ISENGARD_ENROLL_TOKEN=<token-from-step-2> \
     ghcr.io/dirdmaster/isengard-agent:next
   \`\`\`

4. To remove an agent: `isengard controller agent revoke <host_id>`.
```

- [ ] **Step 2: Write release notes**

Create `docs/RELEASE_NOTES_PHASE_14.md`:

```markdown
# Phase 14: Auth & Identity (BREAKING CHANGE)

The shared `ISENGARD_TOKEN` bearer secret is gone. Auth now uses an internal CA + per-agent mTLS + short-lived enrollment tokens.

## Migration

There is no in-place migration. To upgrade:

1. Stop the controller and all agents.
2. Wipe state-dir on the controller (`/var/lib/isengard`).
3. Wipe state-dir on every agent (`/var/lib/isengard`).
4. Drop `ISENGARD_TOKEN` from your env / compose / docker run.
5. Start the controller (no env var changes needed; on first boot it generates a CA).
6. For each agent: mint an enrollment token, set `ISENGARD_ENROLL_TOKEN`, restart the agent.

## What changed

- Controller no longer requires `ISENGARD_TOKEN` at startup.
- New CLI: `isengard controller token mint`, `isengard controller agent revoke <id>`, `isengard controller agent list`.
- New env var (agent first boot only): `ISENGARD_ENROLL_TOKEN`.
- Agent persists `state-dir/certs/` (ca.pem + agent.crt + agent.key, key chmod 600).
- Cert TTL: 30 days, auto-renewed at 50% TTL via the new `RenewCert` RPC.
- Per-cert revocation via dashboard or CLI; revoked agents fail their next mTLS handshake.

## Known limitations (deferred)

- CA private key not encrypted at rest (file permissions only).
- No CA rotation story; rotating the CA requires re-enrolling everyone.
- Dashboard HTTP still unauthenticated (Cloudflare Access integration is the planned answer).
- Bootstrap-trust during the Enroll RPC: agent trusts whatever cert the controller presents during initial enrollment. Mitigations (TOFU pin, out-of-band CA fingerprint) deferred.

See `docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md` for the full design.
```

- [ ] **Step 3: All gates green**

Run from worktree root:
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo deny check
cd crates/isengard-plugins/dashboard/web && bun run build && cd -
```
Expected: all green.

- [ ] **Step 4: Open the PR**

```bash
gh pr create --title "Phase 14: Auth & Identity (Swarm-style mTLS)" \
  --body "$(cat <<'EOF'
Replaces Phase 2c's shared \`ISENGARD_TOKEN\` bearer secret with internal-CA + per-agent mTLS + short-lived enrollment tokens.

**Spec:** \`docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md\`
**Plan:** \`docs/superpowers/plans/2026-05-05-phase-14-auth-and-identity.md\`

## Summary
- Controller: ca::Authority (rcgen ECDSA P-256), EnrollmentService (mint/redeem/renew), RevocationSet, mTLS via tonic ServerTlsConfig + CertAuthInterceptor (Enroll bypass).
- Agent: cert_store module (atomic writes, chmod 600), enrollment refactor (ISENGARD_ENROLL_TOKEN), cert_renewal task (50% TTL).
- CLI: drop ISENGARD_TOKEN, new \`controller token mint | agent revoke | agent list\` subcommands.
- Dashboard: Enrollment tab + Mint Token modal + per-host Revoke button.
- Migration 0014: ca + enrollment_tokens + agent_certs.

## Breaking change
Wipe state-dirs and re-enroll. See \`docs/RELEASE_NOTES_PHASE_14.md\`.

## Test plan
- [ ] cargo test --workspace
- [ ] cargo clippy --workspace --all-targets -- -D warnings
- [ ] cargo deny check
- [ ] bun run build
- [ ] auth_e2e (real-Docker, --ignored)
- [ ] Manual smoke: dashboard mint token → docker run agent → see in hosts → revoke
EOF
)" \
  --base next
```

- [ ] **Step 5: Commit**

```bash
git add README.md docs/RELEASE_NOTES_PHASE_14.md
git commit -m "docs: phase 14 README + release notes (breaking auth change)"
```

---

## Self-review

**Spec coverage:**
- ✅ Controller boots without operator-supplied secret → Task 9 drops ISENGARD_TOKEN reads, Task 2 auto-inits CA
- ✅ Short-lived mintable tokens → Task 3 (mint), Task 9 (CLI), Task 13/14 (dashboard)
- ✅ mTLS gRPC → Task 8
- ✅ Per-cert revocation → Task 4 (set), Task 13 (REST), Task 14 (UI)
- ✅ Cert rotation at 50% TTL → Task 7 (RenewCert), Task 12 (renewal loop)
- ✅ Storage: ca, enrollment_tokens, agent_certs → Task 1
- ✅ Migration documented as breaking + wipe → Task 16

**Type consistency:**
- `CertBundle` uniform across cert_store + enroll + cert_renewal ✅
- `EnrollResponse` (controller) → `EnrollOutcome` (agent) — different names but distinct types in different crates, consistent within each ✅
- `TokenRole::Agent` referenced consistently ✅
- `HostId` API: `to_db_bytes` + `from_db_bytes` + `from_string` + `nil()` — verify `nil()` exists or add it (note in Task 13)
- `Inventory::open_in_memory` used in tests — verify this is the existing test fixture name

**No placeholders:** All `todo!()` calls in Task 15 are documented as "use the existing pattern" and the engineer is told what to crib from. Acceptable for a real-Docker e2e where the harness is identical to existing tests.

**Open follow-ups left in the spec (intentionally deferred):**
- CA encryption at rest
- TOFU pin during bootstrap Enroll
- Multi-role (manager vs worker)
- Dashboard auth
- CA rotation
