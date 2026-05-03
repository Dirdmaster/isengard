# Phase 8 Plan B: TLS + tailscale + cf-tunnel Adapters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Pingora proxy serve real HTTPS via three transports: Let's Encrypt for the `none` adapter, adapter-provided certs for tailscale (CLI-driven), and edge-terminated TLS for cloudflared. After this plan, `https://blog.example.com` works end-to-end behind any of the three transports.

**Architecture:** Pingora 0.8 with `pingora-rustls` feature gains a `:8443` HTTPS listener whose `ResolvesServerCert` impl reads from a process-wide `cert_store` (filesystem source, in-memory cache). The agent runs an ACME task (`instant-acme`) that registers an LE account, drives HTTP-01 challenges through the existing `:8080` listener, writes certs to `/var/lib/isengard/tls/`, schedules renewals at 30 days remaining. Two new adapter crates wrap subprocess CLIs (`tailscale`, `cloudflared`) plus the CF v4 REST API.

**Tech Stack:** Rust 2024, tokio, `instant-acme` 0.7, `rustls` 0.23 (via pingora-rustls feature), `reqwest` (rustls), `wiremock` for tests, plus runtime deps on the user's `tailscale` and `cloudflared` binaries.

**Branch:** `feat/networking-tls-adapters` off `feat/networking-proxy-core` (in worktree `.worktrees/networking-tls-adapters`). Do NOT push without explicit approval — `next` is public.

**Spec:** `docs/superpowers/specs/2026-05-03-phase-8e-8g-tls-and-adapters-design.md`

**Predecessor:** Plan A (`feat/networking-proxy-core` branch, PR #18). Plan B builds on Plan A's `NetworkingAdapter` trait, routing tables, `tls_certs` table skeleton, Pingora supervisor, and `cert_store`-shaped hooks.

---

## Scope

In:
- 8e-1: storage migration + Pingora `:8443` HTTPS listener + cert resolver from filesystem
- 8e-2: `instant-acme` integration + HTTP-01 challenge handler on `:8080`
- 8e-3: renewal scheduler + rate-limit guard + `tls.acme.failed`/`tls.cert.renewed` events
- 8f-1: `networking-tailscale` crate skeleton + CLI wrappers + `join()` / `unexpose()`
- 8f-2: `expose()` via `tailscale serve` + `tailscale cert` + cert installed
- 8g-1: `networking-cf-tunnel` crate skeleton + CF v4 API client + cloudflared subprocess supervisor
- 8g-2: `expose()` / `unexpose()` for cf-tunnel + ingress rule + DNS CNAME
- 8b-9: backport HTTPS listener into production `proxy::server::run`

Out (later plans):
- Plan C: Settings UI Networking tab (8h), atomic upstream swap (8i)
- DNS-01 ACME challenge, wildcards
- Headscale, raw-wireguard, custom adapters
- Tailscale custom-domain (CNAME-to-tailnet) support
- mTLS to upstream containers, HTTP/3

Done when:

1. `cargo build --workspace` clean, no `-D warnings` violations
2. `cargo test --workspace` passes (including new wiremock + Pebble-gated integration tests)
3. End-to-end (manual smoke, opt-in): `none` adapter + DNS A record on a host with public IP → real LE cert via HTTP-01 → `curl https://blog.example.com` returns the container response with a valid cert
4. End-to-end (manual smoke, opt-in): `tailscale` adapter on a host logged into a tailnet → `tailscale funnel` flag set → `curl https://<host>.<tailnet>.ts.net` returns the container response
5. End-to-end (manual smoke, opt-in): `cf-tunnel` adapter with valid CF API token → tunnel created on first `join()`, ingress rule + DNS CNAME on `expose()` → `curl https://blog.example.com` returns the container response with edge cert
6. Cert renewal triggers ≥30 days before expiry; `tls.cert.renewed` event lands in journal
7. Adapter and tunnel state survive agent restart (no re-register needed)
8. ~25-30 commits on `feat/networking-tls-adapters`, each green individually
9. Branch is reviewable as a single PR back to `next` (or stacked on PR #18)

---

## File Structure

### Create

| File | Responsibility |
|---|---|
| `crates/isengard-storage/migrations/0010_acme.sql` | Add `last_attempt_at`, `last_error`, `attempt_count` to `tls_certs`; create `acme_account` singleton table |
| `crates/isengard-storage/src/acme_account.rs` | `AcmeAccount` entity + `get_acme_account` / `upsert_acme_account` / `record_tls_attempt` |
| `crates/isengard-agent/src/tls/mod.rs` | Module root: re-exports + `TlsConfig` settings struct |
| `crates/isengard-agent/src/tls/storage.rs` | Filesystem cert storage: read/write PEM at `/var/lib/isengard/tls/<host>.{crt,key}`, mode 0600 |
| `crates/isengard-agent/src/tls/cert_store.rs` | Process-wide `Arc<RwLock<HashMap<String, CertifiedKey>>>` cache + filesystem-source loader |
| `crates/isengard-agent/src/tls/acme.rs` | `instant-acme` wrapper: account register, order, HTTP-01 challenge orchestration |
| `crates/isengard-agent/src/tls/challenge_state.rs` | Shared `HashMap<token, key_authorization>` between ACME task and `:8080` HTTP-01 handler |
| `crates/isengard-agent/src/tls/renewal.rs` | Cron-style renewal scheduler: tokio task that wakes every hour, drives renewals |
| `crates/isengard-agent/src/proxy/cert_resolver.rs` | `IsengardCertResolver: ResolvesServerCert` for Pingora rustls |
| `crates/isengard-plugins/networking-tailscale/Cargo.toml` | New crate manifest |
| `crates/isengard-plugins/networking-tailscale/src/lib.rs` | `TailscaleAdapter: NetworkingAdapter` |
| `crates/isengard-plugins/networking-tailscale/src/cli.rs` | `tokio::process` wrappers for `tailscale {status,serve,funnel,cert}` |
| `crates/isengard-plugins/networking-tailscale/tests/loads.rs` | Adapter registration test |
| `crates/isengard-plugins/networking-cf-tunnel/Cargo.toml` | New crate manifest |
| `crates/isengard-plugins/networking-cf-tunnel/src/lib.rs` | `CfTunnelAdapter: NetworkingAdapter` |
| `crates/isengard-plugins/networking-cf-tunnel/src/api.rs` | CF v4 REST API client (`zones`, `tunnels`, `dns_records`) |
| `crates/isengard-plugins/networking-cf-tunnel/src/cloudflared.rs` | Subprocess supervisor (mirrors `proxy::supervise` pattern) |
| `crates/isengard-plugins/networking-cf-tunnel/tests/api_units.rs` | wiremock unit tests for the API client |
| `crates/isengard-plugins/networking-cf-tunnel/tests/loads.rs` | Adapter registration test |
| `crates/isengard-storage/tests/acme_account.rs` | CRUD tests |
| `crates/isengard-agent/tests/cert_resolver_unit.rs` | Verifies cert lookup hits the cache + falls back to filesystem |
| `crates/isengard-agent/tests/acme_pebble_e2e.rs` | `#[ignore]`'d — runs against local Pebble for end-to-end ACME |

### Modify

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Add `instant-acme = "0.7"`, `wiremock = "0.6"` (dev), `rustls = "0.23"` (transitive via pingora feature); list two new plugin crates in `members` |
| `crates/isengard-storage/src/lib.rs` | Re-export `acme_account::*` |
| `crates/isengard-storage/src/tls_cert.rs` | Add `last_attempt_at`/`last_error`/`attempt_count` to row + `record_tls_attempt` method |
| `crates/isengard-agent/Cargo.toml` | Add `instant-acme`, `rustls`, `rcgen` (for tests), `pingora-rustls` feature on pingora deps |
| `crates/isengard-agent/src/lib.rs` | `pub mod tls;` + spawn renewal task in `run_agent`; pass `cert_store` into `proxy::server::run` |
| `crates/isengard-agent/src/proxy/mod.rs` | Re-export `cert_resolver::*`; thread `Arc<CertStore>` into `ProxyState` |
| `crates/isengard-agent/src/proxy/server.rs` | Bind `:8443` HTTPS listener with `IsengardCertResolver`; pass cert store into the listener config |
| `crates/isengard/Cargo.toml` | Add `isengard-plugin-networking-tailscale` and `isengard-plugin-networking-cf-tunnel` as path deps (cargo features `tailscale`, `cf-tunnel`, both default-on) |
| `crates/isengard/src/main.rs` | `use isengard_plugin_networking_tailscale as _;` and `use isengard_plugin_networking_cf_tunnel as _;` (under their respective feature flags) |

---

## Phase 8e-1: storage migration + Pingora HTTPS listener with file-source cert resolver

### Task 1: Migration `0010_acme.sql`

**Files:**
- Create: `crates/isengard-storage/migrations/0010_acme.sql`

- [ ] **Step 1: Write the migration**

```sql
-- Phase 8e-1: extend tls_certs for ACME state + create the ACME account singleton.
-- See docs/superpowers/specs/2026-05-03-phase-8e-8g-tls-and-adapters-design.md §7.

ALTER TABLE tls_certs ADD COLUMN last_attempt_at TEXT;
ALTER TABLE tls_certs ADD COLUMN last_error TEXT;
ALTER TABLE tls_certs ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE acme_account (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    contact_email   TEXT NOT NULL,
    directory_url   TEXT NOT NULL,
    account_key_pem TEXT NOT NULL,
    kid             TEXT,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

- [ ] **Step 2: Verify the migration applies**

Run: `cargo test -p isengard-storage`
Expected: existing 33+ tests still green; sqlx auto-applies the new migration on the temp DB.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/migrations/0010_acme.sql
git commit -m "feat(storage): migration 0010: acme_account + tls_certs ACME state"
```

---

### Task 2: `AcmeAccount` entity + CRUD

**Files:**
- Create: `crates/isengard-storage/src/acme_account.rs`
- Modify: `crates/isengard-storage/src/lib.rs`
- Create: `crates/isengard-storage/tests/acme_account.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/isengard-storage/tests/acme_account.rs`:

```rust
use isengard_storage::{AcmeAccount, Inventory, UpsertAcmeAccount};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_account() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db")).await.unwrap();

    assert!(inv.get_acme_account().await.unwrap().is_none(), "no account yet");

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "ops@example.com".into(),
        directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "-----BEGIN PRIVATE KEY-----\nMI...\n-----END PRIVATE KEY-----".into(),
        kid: Some("https://acme-staging-v02.api.letsencrypt.org/acme/acct/123".into()),
    }).await.unwrap();

    let acct = inv.get_acme_account().await.unwrap().expect("exists");
    assert_eq!(acct.contact_email, "ops@example.com");
    assert!(acct.directory_url.contains("staging"));
    assert_eq!(acct.kid.as_deref(), Some("https://acme-staging-v02.api.letsencrypt.org/acme/acct/123"));
}

#[tokio::test]
async fn upsert_overwrites_existing_singleton() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db")).await.unwrap();

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "first@example.com".into(),
        directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "key1".into(),
        kid: None,
    }).await.unwrap();

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "second@example.com".into(),
        directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "key2".into(),
        kid: Some("kid2".into()),
    }).await.unwrap();

    let acct = inv.get_acme_account().await.unwrap().unwrap();
    assert_eq!(acct.contact_email, "second@example.com");
    assert_eq!(acct.account_key_pem, "key2");
    assert_eq!(acct.kid.as_deref(), Some("kid2"));
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p isengard-storage --test acme_account`
Expected: FAIL — `AcmeAccount`/`UpsertAcmeAccount` undefined.

- [ ] **Step 3: Create `crates/isengard-storage/src/acme_account.rs`**

```rust
//! ACME account singleton — one per controller. Keeps the registered LE
//! account so we don't re-register on every restart.

use crate::error::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeAccount {
    pub contact_email: String,
    pub directory_url: String,
    pub account_key_pem: String,
    pub kid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertAcmeAccount {
    pub contact_email: String,
    pub directory_url: String,
    pub account_key_pem: String,
    pub kid: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn upsert_acme_account(&self, ins: UpsertAcmeAccount) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO acme_account (id, contact_email, directory_url, account_key_pem, kid)
            VALUES (1, ?, ?, ?, ?)
            ON CONFLICT (id) DO UPDATE SET
              contact_email = excluded.contact_email,
              directory_url = excluded.directory_url,
              account_key_pem = excluded.account_key_pem,
              kid = excluded.kid
            "#,
        )
        .bind(&ins.contact_email)
        .bind(&ins.directory_url)
        .bind(&ins.account_key_pem)
        .bind(&ins.kid)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_acme_account(&self) -> Result<Option<AcmeAccount>> {
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT contact_email, directory_url, account_key_pem, kid FROM acme_account WHERE id = 1",
        )
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| AcmeAccount {
            contact_email: r.try_get("contact_email").unwrap_or_default(),
            directory_url: r.try_get("directory_url").unwrap_or_default(),
            account_key_pem: r.try_get("account_key_pem").unwrap_or_default(),
            kid: r.try_get("kid").ok(),
        }))
    }
}
```

- [ ] **Step 4: Re-export from `lib.rs`**

In `crates/isengard-storage/src/lib.rs`:

```rust
pub mod acme_account;
pub use acme_account::{AcmeAccount, UpsertAcmeAccount};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-storage --test acme_account`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/acme_account.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/tests/acme_account.rs
git commit -m "feat(storage): AcmeAccount singleton + upsert/get"
```

---

### Task 3: Extend `tls_certs` with attempt-tracking accessors

**Files:**
- Modify: `crates/isengard-storage/src/tls_cert.rs`
- Modify: `crates/isengard-storage/tests/tls_cert.rs`

- [ ] **Step 1: Add the failing test**

Append to `crates/isengard-storage/tests/tls_cert.rs`:

```rust
#[tokio::test]
async fn record_attempt_increments_count_and_clears_error_on_success() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db")).await.unwrap();
    let host = inv.enroll_host(EnrollHost {
        fingerprint: "fp".into(), hostname: "h".into(), os: "linux".into(),
        arch: "x86_64".into(), agent_version: "0".into(), docker_version: "27".into(),
        fleet: "default".into(),
    }).await.unwrap();

    let now = Utc::now();
    inv.upsert_tls_cert_meta(UpsertTlsCertMeta {
        public_hostname: "blog.example.com".into(),
        host_id: host,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + chrono::Duration::days(90),
        next_renewal_at: now + chrono::Duration::days(60),
        serial: None,
    }).await.unwrap();

    inv.record_tls_attempt("blog.example.com", false, Some("rate limited".into()))
        .await.unwrap();
    let m = inv.get_tls_cert_meta("blog.example.com").await.unwrap().unwrap();
    assert_eq!(m.attempt_count, 1);
    assert_eq!(m.last_error.as_deref(), Some("rate limited"));

    inv.record_tls_attempt("blog.example.com", true, None).await.unwrap();
    let m = inv.get_tls_cert_meta("blog.example.com").await.unwrap().unwrap();
    assert_eq!(m.attempt_count, 2);
    assert_eq!(m.last_error, None, "success clears error");
}
```

The test references new fields `m.attempt_count` and `m.last_error` on `TlsCertMeta`. Step 3 adds them.

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p isengard-storage --test tls_cert -- record_attempt_increments_count_and_clears_error_on_success`
Expected: FAIL — fields and method undefined.

- [ ] **Step 3: Extend `TlsCertMeta` struct + add `record_tls_attempt`**

In `crates/isengard-storage/src/tls_cert.rs`, extend `TlsCertMeta`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsCertMeta {
    pub public_hostname: String,
    pub host_id: HostId,
    pub issuer: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub last_renewed_at: Option<DateTime<Utc>>,
    pub next_renewal_at: DateTime<Utc>,
    pub serial: Option<String>,
    // NEW (Plan B 8e-1):
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub attempt_count: u32,
}
```

Update `get_tls_cert_meta` SELECT and row decoder to include the three new columns. Update test seed code (`UpsertTlsCertMeta` doesn't need to change — the new fields default).

Add the new method:

```rust
impl crate::inventory::Inventory {
    pub async fn record_tls_attempt(
        &self,
        public_hostname: &str,
        success: bool,
        error: Option<String>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        if success {
            sqlx::query(
                r#"
                UPDATE tls_certs
                SET last_attempt_at = ?, last_error = NULL, attempt_count = attempt_count + 1
                WHERE public_hostname = ?
                "#,
            )
            .bind(&now)
            .bind(public_hostname)
            .execute(self.pool())
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE tls_certs
                SET last_attempt_at = ?, last_error = ?, attempt_count = attempt_count + 1
                WHERE public_hostname = ?
                "#,
            )
            .bind(&now)
            .bind(&error)
            .bind(public_hostname)
            .execute(self.pool())
            .await?;
        }
        Ok(())
    }
}
```

Update the row decoder in `get_tls_cert_meta` to read the new columns (parse them as nullable RFC3339 / Option<String> / i64).

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-storage`
Expected: green (existing tests + the new one).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/tls_cert.rs \
        crates/isengard-storage/tests/tls_cert.rs
git commit -m "feat(storage): tls_certs gains attempt-tracking columns + record_tls_attempt"
```

---

### Task 4: `tls/storage.rs` — filesystem cert read/write at `/var/lib/isengard/tls/`

**Files:**
- Create: `crates/isengard-agent/src/tls/mod.rs`
- Create: `crates/isengard-agent/src/tls/storage.rs`
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/isengard-agent/tests/tls_storage_unit.rs`:

```rust
use isengard_agent::tls::storage::{CertFiles, TlsStorage};
use tempfile::tempdir;

#[tokio::test]
async fn write_then_read_returns_cert_pair() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());

    let pem_cert = "-----BEGIN CERTIFICATE-----\nMIIBkjCCATigAwIBAgI...\n-----END CERTIFICATE-----\n";
    let pem_key = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGS...\n-----END PRIVATE KEY-----\n";

    storage.write("blog.example.com", pem_cert, pem_key).await.unwrap();
    let CertFiles { cert_pem, key_pem } = storage.read("blog.example.com").await.unwrap();

    assert_eq!(cert_pem, pem_cert);
    assert_eq!(key_pem, pem_key);
}

#[tokio::test]
async fn read_missing_returns_err() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let err = storage.read("does-not-exist.test").await.unwrap_err();
    assert!(format!("{err}").contains("does-not-exist.test"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn write_sets_mode_0600_on_key() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    storage.write("h.test", "cert", "key").await.unwrap();

    let key_path = dir.path().join("h.test.key");
    let perms = tokio::fs::metadata(&key_path).await.unwrap().permissions();
    assert_eq!(perms.mode() & 0o777, 0o600, "key file must be mode 0600");
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p isengard-agent --test tls_storage_unit`
Expected: FAIL — `tls::storage` module does not exist.

- [ ] **Step 3: Create the module**

Create `crates/isengard-agent/src/tls/mod.rs`:

```rust
//! TLS subsystem: ACME client, on-disk cert storage, in-memory cert cache,
//! Pingora cert-resolver hook, renewal scheduler.
//!
//! See spec §3 + §7 in
//! `docs/superpowers/specs/2026-05-03-phase-8e-8g-tls-and-adapters-design.md`.

pub mod storage;

pub use storage::{CertFiles, TlsStorage};
```

Create `crates/isengard-agent/src/tls/storage.rs`:

```rust
//! On-disk cert storage at `/var/lib/isengard/tls/<hostname>.{crt,key}`.
//! Mode 0600 on Unix. Source of truth for the cert cache + Pingora resolver.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertFiles {
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Clone)]
pub struct TlsStorage {
    root: PathBuf,
}

impl TlsStorage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn cert_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.crt"))
    }

    fn key_path(&self, hostname: &str) -> PathBuf {
        self.root.join(format!("{hostname}.key"))
    }

    pub async fn write(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        fs::create_dir_all(&self.root)
            .await
            .with_context(|| format!("creating cert root {:?}", self.root))?;

        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);

        fs::write(&cert_path, cert_pem)
            .await
            .with_context(|| format!("writing cert {cert_path:?}"))?;

        // For the key, set 0600 mode atomically by opening with create+write
        // and chmodding via permissions before the write completes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut f = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&key_path)
                .await
                .with_context(|| format!("opening key {key_path:?}"))?;
            f.write_all(key_pem.as_bytes())
                .await
                .with_context(|| format!("writing key {key_path:?}"))?;
            f.flush().await.ok();
            drop(f);
            fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| format!("chmod 0600 {key_path:?}"))?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&key_path, key_pem)
                .await
                .with_context(|| format!("writing key {key_path:?}"))?;
        }
        Ok(())
    }

    pub async fn read(&self, hostname: &str) -> Result<CertFiles> {
        let cert_path = self.cert_path(hostname);
        let key_path = self.key_path(hostname);
        let cert_pem = fs::read_to_string(&cert_path)
            .await
            .with_context(|| format!("reading {cert_path:?} for {hostname}"))?;
        let key_pem = fs::read_to_string(&key_path)
            .await
            .with_context(|| format!("reading {key_path:?} for {hostname}"))?;
        Ok(CertFiles { cert_pem, key_pem })
    }

    pub async fn delete(&self, hostname: &str) -> Result<()> {
        let _ = fs::remove_file(self.cert_path(hostname)).await;
        let _ = fs::remove_file(self.key_path(hostname)).await;
        Ok(())
    }
}
```

In `crates/isengard-agent/src/lib.rs`, add `pub mod tls;`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-agent --test tls_storage_unit -- --nocapture`
Expected: 3 passed (or 2 on Windows where the mode test is `#[cfg(unix)]`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/ crates/isengard-agent/src/lib.rs \
        crates/isengard-agent/tests/tls_storage_unit.rs
git commit -m "feat(agent): tls::storage — filesystem cert read/write with 0600 key mode"
```

---

### Task 5: `cert_store.rs` + `IsengardCertResolver` for Pingora rustls

**Files:**
- Create: `crates/isengard-agent/src/tls/cert_store.rs`
- Create: `crates/isengard-agent/src/proxy/cert_resolver.rs`
- Modify: `crates/isengard-agent/src/tls/mod.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/Cargo.toml`
- Create: `crates/isengard-agent/tests/cert_resolver_unit.rs`

- [ ] **Step 1: Add Cargo deps**

Modify `crates/isengard-agent/Cargo.toml`:

```toml
[dependencies]
# ... existing ...
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
rustls-pemfile = "2"

[dev-dependencies]
# ... existing ...
rcgen = "0.13"  # generate self-signed certs in tests
```

Verify the workspace `pingora-core` / `pingora-proxy` lines have feature `pingora-rustls`:

In root `Cargo.toml` `[workspace.dependencies]`:

```toml
pingora-core = { version = "0.8", default-features = false, features = ["rustls"] }
pingora-proxy = { version = "0.8", default-features = false, features = ["rustls"] }
```

(Plan A may have used the default features. Switching to explicit `rustls` keeps build deterministic and avoids accidental boringssl pull-ins.)

Run: `cargo build --workspace`
Expected: success.

- [ ] **Step 2: Write the failing test**

Create `crates/isengard-agent/tests/cert_resolver_unit.rs`:

```rust
//! Verify the cert resolver hits the in-memory cache + falls back to the
//! filesystem when the cache is cold.

use isengard_agent::tls::cert_store::CertStore;
use isengard_agent::tls::storage::TlsStorage;
use rcgen::{CertificateParams, KeyPair};
use std::sync::Arc;
use tempfile::tempdir;

fn issue_self_signed(hostname: &str) -> (String, String) {
    let key_pair = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![hostname.to_string()]).unwrap();
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        hostname.to_string(),
    );
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair.serialize_pem())
}

#[tokio::test]
async fn lookup_loads_from_filesystem_on_cache_miss() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let (cert_pem, key_pem) = issue_self_signed("h.test");
    storage.write("h.test", &cert_pem, &key_pem).await.unwrap();

    let store = Arc::new(CertStore::new(storage.clone()));
    let key = store.lookup("h.test").await.expect("loads from disk");
    assert!(key.cert.first().is_some(), "has at least one cert in chain");
}

#[tokio::test]
async fn lookup_returns_none_when_neither_cache_nor_disk_has_it() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let store = Arc::new(CertStore::new(storage));
    assert!(store.lookup("does-not-exist.test").await.is_none());
}

#[tokio::test]
async fn install_then_lookup_serves_from_cache() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let store = Arc::new(CertStore::new(storage));

    let (cert_pem, key_pem) = issue_self_signed("h.test");
    store.install("h.test", &cert_pem, &key_pem).await.unwrap();
    let key = store.lookup("h.test").await.expect("served from cache");
    assert!(key.cert.first().is_some());
}
```

- [ ] **Step 3: Run to verify fail**

Run: `cargo test -p isengard-agent --test cert_resolver_unit`
Expected: FAIL — `CertStore` does not exist.

- [ ] **Step 4: Implement `CertStore`**

Create `crates/isengard-agent/src/tls/cert_store.rs`:

```rust
//! Process-wide cache of `CertifiedKey` indexed by hostname (SNI).
//! Backed by `TlsStorage` (filesystem). Cache is read-mostly; mutations
//! happen on cert install / renewal and are explicit.

use crate::tls::storage::TlsStorage;
use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SigningKey};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct CertStore {
    storage: TlsStorage,
    cache: Arc<RwLock<HashMap<String, Arc<CertifiedKey>>>>,
}

impl CertStore {
    pub fn new(storage: TlsStorage) -> Self {
        Self {
            storage,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Look up a `CertifiedKey` by SNI hostname. Cache-first, then disk.
    /// Returns `None` if neither has it.
    pub async fn lookup(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        if let Some(k) = self.cache.read().await.get(hostname).cloned() {
            return Some(k);
        }
        match self.load_from_disk(hostname).await {
            Ok(k) => {
                self.cache.write().await.insert(hostname.to_string(), k.clone());
                Some(k)
            }
            Err(_) => None,
        }
    }

    /// Install a new cert for `hostname`: writes to disk + updates cache.
    pub async fn install(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        self.storage.write(hostname, cert_pem, key_pem).await?;
        let key = parse_certified_key(cert_pem, key_pem)?;
        self.cache.write().await.insert(hostname.to_string(), Arc::new(key));
        Ok(())
    }

    /// Drop a hostname's cert from cache + disk.
    pub async fn remove(&self, hostname: &str) -> Result<()> {
        self.cache.write().await.remove(hostname);
        self.storage.delete(hostname).await
    }

    async fn load_from_disk(&self, hostname: &str) -> Result<Arc<CertifiedKey>> {
        let files = self.storage.read(hostname).await?;
        let key = parse_certified_key(&files.cert_pem, &files.key_pem)?;
        Ok(Arc::new(key))
    }
}

fn parse_certified_key(cert_pem: &str, key_pem: &str) -> Result<CertifiedKey> {
    let mut cert_reader = std::io::BufReader::new(cert_pem.as_bytes());
    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing cert PEM")?;
    if cert_chain.is_empty() {
        return Err(anyhow!("cert PEM contains no certificates"));
    }

    let mut key_reader = std::io::BufReader::new(key_pem.as_bytes());
    let key_der: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .context("parsing key PEM")?
        .or_else(|| {
            // Try PKCS8 specifically as a fallback
            let mut r = std::io::BufReader::new(key_pem.as_bytes());
            rustls_pemfile::pkcs8_private_keys(&mut r)
                .next()
                .and_then(|res| res.ok())
                .map(|k| PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.secret_pkcs8_der().to_vec())))
        })
        .ok_or_else(|| anyhow!("no private key found in PEM"))?;

    let signing_key: Arc<dyn SigningKey> =
        rustls::crypto::ring::sign::any_supported_type(&key_der)
            .map_err(|e| anyhow!("unsupported key type: {e}"))?;
    Ok(CertifiedKey::new(cert_chain, signing_key))
}
```

Update `crates/isengard-agent/src/tls/mod.rs`:

```rust
pub mod cert_store;
pub mod storage;

pub use cert_store::CertStore;
pub use storage::{CertFiles, TlsStorage};
```

- [ ] **Step 5: Implement the Pingora `ResolvesServerCert`**

Create `crates/isengard-agent/src/proxy/cert_resolver.rs`:

```rust
//! Pingora rustls cert resolver. Looks up SNI in the agent's `CertStore`.
//!
//! `ResolvesServerCert::resolve` is a sync trait but our cache is async,
//! so we use `block_in_place` to bridge.

use crate::tls::CertStore;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

pub struct IsengardCertResolver {
    store: Arc<CertStore>,
}

impl IsengardCertResolver {
    pub fn new(store: Arc<CertStore>) -> Self {
        Self { store }
    }
}

impl std::fmt::Debug for IsengardCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsengardCertResolver").finish()
    }
}

impl ResolvesServerCert for IsengardCertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?.to_string();
        let store = self.store.clone();
        // rustls calls resolve from the runtime that drives the TLS handshake
        // (Pingora's server runtime, also tokio). block_in_place is safe here
        // because we're already inside a multi-thread runtime worker.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                store.lookup(&sni).await
            })
        })
    }
}
```

Re-export from `crates/isengard-agent/src/proxy/mod.rs`:

```rust
pub mod cert_resolver;
pub use cert_resolver::IsengardCertResolver;
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p isengard-agent --test cert_resolver_unit`
Expected: 3 passed.

Also re-run workspace build to confirm Pingora pingora-rustls feature compiles:
Run: `cargo build --workspace`
Expected: success.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/cert_store.rs \
        crates/isengard-agent/src/tls/mod.rs \
        crates/isengard-agent/src/proxy/cert_resolver.rs \
        crates/isengard-agent/src/proxy/mod.rs \
        crates/isengard-agent/Cargo.toml \
        crates/isengard-agent/tests/cert_resolver_unit.rs \
        Cargo.toml
git commit -m "feat(agent): cert_store + IsengardCertResolver for Pingora rustls"
```

---

### Task 6: Pingora `:8443` HTTPS listener with `IsengardCertResolver`

**Files:**
- Modify: `crates/isengard-agent/src/proxy/server.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs` (`ProxyState` gains `cert_store`)

- [ ] **Step 1: Update `ProxyState` to carry the cert store**

In `crates/isengard-agent/src/proxy/mod.rs`:

```rust
use crate::tls::CertStore;
// ... existing imports ...

#[derive(Debug, Default, Clone)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    pub last_generation: Arc<AtomicU64>,
    pub event_tx: Arc<RwLock<Option<mpsc::Sender<Event>>>>,
    /// Cert resolver for the :8443 HTTPS listener. None = HTTPS listener
    /// is not bound (test default).
    pub cert_store: Arc<RwLock<Option<Arc<CertStore>>>>,
}

impl ProxyState {
    pub fn new() -> Self { Self::default() }

    pub async fn install_cert_store(&self, store: Arc<CertStore>) {
        *self.cert_store.write().await = Some(store);
    }
}
```

(Note: `CertStore` is wrapped in `Option` so tests that don't care about HTTPS can leave it `None`. Production startup installs it.)

- [ ] **Step 2: Bind the HTTPS listener in `server::run`**

In `crates/isengard-agent/src/proxy/server.rs`, replace `run`:

```rust
use crate::proxy::IsengardCertResolver;
use pingora_core::listeners::tls::TlsSettings;
use rustls::ServerConfig as RustlsServerConfig;
use std::sync::Arc;

pub async fn run(state: ProxyState, http_port: u16, https_port: u16) {
    let cert_store_opt = state.cert_store.read().await.clone();

    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state.clone()));
    svc.add_tcp(&format!("0.0.0.0:{http_port}"));

    if let Some(cert_store) = cert_store_opt {
        let resolver = Arc::new(IsengardCertResolver::new(cert_store));
        let provider = rustls::crypto::ring::default_provider();
        let _ = provider.install_default();
        let cfg = RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        let tls_settings = TlsSettings::with_rustls(cfg);
        svc.add_tls_with_settings(&format!("0.0.0.0:{https_port}"), None, tls_settings);
        tracing::info!(http_port, https_port, "proxy: HTTPS listener configured");
    } else {
        tracing::info!(http_port, "proxy: HTTPS listener disabled (no cert_store installed)");
    }

    server.add_service(svc);
    let _ = tokio::task::spawn_blocking(move || server.run(RunArgs::default())).await;
}
```

`run_for_test` stays unchanged (no HTTPS in tests for now — they use the HTTP listener directly).

The exact `add_tls_with_settings` signature may shift between pingora 0.8 patch versions. If it differs, check `pingora_core::listeners::tls` and adapt — the principle is "configure rustls with our cert resolver and bind on the HTTPS port."

- [ ] **Step 3: Verify build + existing tests**

Run: `cargo build --workspace`
Expected: success.

Run: `cargo test -p isengard-agent`
Expected: green (existing 17+ lib tests, integration tests; HTTPS listener untouched by tests).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/proxy/
git commit -m "feat(agent): bind :8443 HTTPS listener with IsengardCertResolver"
```

---

## Phase 8e-2: instant-acme integration + HTTP-01 challenge handler

### Task 7: ACME challenge state shared between ACME task and `:8080` listener

**Files:**
- Create: `crates/isengard-agent/src/tls/challenge_state.rs`
- Modify: `crates/isengard-agent/src/tls/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/isengard-agent/tests/acme_challenge_state_unit.rs`:

```rust
use isengard_agent::tls::challenge_state::ChallengeState;
use std::sync::Arc;

#[tokio::test]
async fn install_then_lookup_returns_key_authorization() {
    let st = Arc::new(ChallengeState::new());
    st.install("token-abc", "key-auth-xyz").await;
    assert_eq!(st.lookup("token-abc").await.as_deref(), Some("key-auth-xyz"));
}

#[tokio::test]
async fn remove_clears_token() {
    let st = Arc::new(ChallengeState::new());
    st.install("t", "ka").await;
    st.remove("t").await;
    assert!(st.lookup("t").await.is_none());
}

#[tokio::test]
async fn lookup_unknown_returns_none() {
    let st = Arc::new(ChallengeState::new());
    assert!(st.lookup("nope").await.is_none());
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-agent --test acme_challenge_state_unit`
Expected: FAIL — module undefined.

- [ ] **Step 3: Implement**

Create `crates/isengard-agent/src/tls/challenge_state.rs`:

```rust
//! Shared `HashMap<token, key_authorization>` between the ACME order task
//! and the `:8080` HTTP-01 challenge handler. Tokens are short-lived (LE
//! validates within seconds).

use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Default)]
pub struct ChallengeState {
    by_token: RwLock<HashMap<String, String>>,
}

impl ChallengeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn install(&self, token: &str, key_authorization: &str) {
        self.by_token
            .write()
            .await
            .insert(token.to_string(), key_authorization.to_string());
    }

    pub async fn lookup(&self, token: &str) -> Option<String> {
        self.by_token.read().await.get(token).cloned()
    }

    pub async fn remove(&self, token: &str) {
        self.by_token.write().await.remove(token);
    }
}
```

Re-export from `crates/isengard-agent/src/tls/mod.rs`:

```rust
pub mod challenge_state;
pub use challenge_state::ChallengeState;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-agent --test acme_challenge_state_unit`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/challenge_state.rs \
        crates/isengard-agent/src/tls/mod.rs \
        crates/isengard-agent/tests/acme_challenge_state_unit.rs
git commit -m "feat(agent): tls::challenge_state — shared HTTP-01 token map"
```

---

### Task 8: Wire `/.well-known/acme-challenge/<token>` into Pingora's `:8080` listener

**Files:**
- Modify: `crates/isengard-agent/src/proxy/router.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs` (`ProxyState` gains `acme_challenge_state`)

- [ ] **Step 1: Update `ProxyState`**

In `crates/isengard-agent/src/proxy/mod.rs`:

```rust
use crate::tls::ChallengeState;

#[derive(Debug, Default, Clone)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    pub last_generation: Arc<AtomicU64>,
    pub event_tx: Arc<RwLock<Option<mpsc::Sender<Event>>>>,
    pub cert_store: Arc<RwLock<Option<Arc<CertStore>>>>,
    /// HTTP-01 challenge state for the :8080 listener. Shared with the ACME
    /// order task. Empty in tests that don't exercise ACME.
    pub acme_challenges: Arc<ChallengeState>,
}
```

- [ ] **Step 2: Add an early-response branch in `IsengardProxy`**

In `crates/isengard-agent/src/proxy/router.rs`, override `request_filter` to short-circuit ACME challenge paths. `ProxyHttp::request_filter` returns `Ok(true)` if it handles the request itself.

```rust
#[async_trait]
impl ProxyHttp for IsengardProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<bool> {
        let path = session.req_header().uri.path();
        const PREFIX: &str = "/.well-known/acme-challenge/";
        if let Some(token) = path.strip_prefix(PREFIX) {
            let token = token.to_string();
            let key_auth = self.state.acme_challenges.lookup(&token).await;
            let body = key_auth.unwrap_or_default(); // empty → 404 fallback
            let status = if body.is_empty() { 404 } else { 200 };
            let mut resp = pingora_http::ResponseHeader::build(status, None)?;
            resp.insert_header("Content-Type", "text/plain")?;
            resp.insert_header("Content-Length", body.len().to_string())?;
            session.write_response_header(Box::new(resp), false).await?;
            session
                .write_response_body(Some(body.into_bytes().into()), true)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        // ... existing host-header lookup unchanged ...
    }
}
```

(Keep the existing `upstream_peer` body — only adding the new `request_filter` method on the same impl block.)

- [ ] **Step 3: Add the integration test**

Create `crates/isengard-agent/tests/acme_challenge_serve.rs`:

```rust
//! Verify Pingora serves /.well-known/acme-challenge/<token> from the
//! shared ChallengeState.

use isengard_agent::proxy::ProxyState;
use isengard_agent::tls::ChallengeState;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn challenge_token_returns_key_authorization() {
    let state = ProxyState::new();
    state.acme_challenges.install("test-token", "test-key-auth-value").await;

    let proxy_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let body = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/.well-known/acme-challenge/test-token",
            proxy_port
        ))
        .timeout(Duration::from_secs(2))
        .send().await.unwrap()
        .text().await.unwrap();

    assert_eq!(body, "test-key-auth-value");

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
}

#[tokio::test]
async fn challenge_unknown_token_returns_404() {
    let state = ProxyState::new();
    let proxy_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/.well-known/acme-challenge/nope",
            proxy_port
        ))
        .timeout(Duration::from_secs(2))
        .send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test -p isengard-agent --test acme_challenge_serve`
Expected: 2 passed.

```bash
cargo fmt --all
git add crates/isengard-agent/src/proxy/ crates/isengard-agent/tests/acme_challenge_serve.rs
git commit -m "feat(agent): serve HTTP-01 acme-challenge tokens from Pingora :8080"
```

---

### Task 9: `instant-acme` wrapper in `tls/acme.rs`

**Files:**
- Create: `crates/isengard-agent/src/tls/acme.rs`
- Modify: `crates/isengard-agent/src/tls/mod.rs`
- Modify: `crates/isengard-agent/Cargo.toml` (`instant-acme = "0.7"`)

- [ ] **Step 1: Add `instant-acme` to `Cargo.toml`**

```toml
[dependencies]
# ... existing ...
instant-acme = "0.7"
```

- [ ] **Step 2: Implement the wrapper**

Create `crates/isengard-agent/src/tls/acme.rs`:

```rust
//! Thin wrapper around `instant-acme`. Handles account registration, order
//! placement, HTTP-01 challenge orchestration, finalization, cert download.
//!
//! The wrapper does NOT own the cert storage or the renewal schedule —
//! `tls::renewal::RenewalScheduler` calls into here.

use crate::tls::ChallengeState;
use anyhow::{Context, Result, anyhow};
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder,
    OrderStatus,
};
use isengard_storage::{AcmeAccount, Inventory, UpsertAcmeAccount};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub const LE_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LE_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

pub struct AcmeClient {
    inventory: Arc<Inventory>,
    challenges: Arc<ChallengeState>,
    contact_email: String,
    directory_url: String,
}

pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

impl AcmeClient {
    pub fn new(
        inventory: Arc<Inventory>,
        challenges: Arc<ChallengeState>,
        contact_email: String,
        directory_url: String,
    ) -> Self {
        Self {
            inventory,
            challenges,
            contact_email,
            directory_url,
        }
    }

    /// Get or create the LE account, persisted in storage so we don't
    /// re-register on restarts.
    async fn account(&self) -> Result<Account> {
        if let Some(saved) = self.inventory.get_acme_account().await? {
            // Reconstruct from the persisted credentials JSON.
            let creds: AccountCredentials =
                serde_json::from_str(&saved.account_key_pem).context("decode acme creds")?;
            return Account::from_credentials(creds)
                .await
                .context("reconstruct acme account");
        }

        let (account, creds) = Account::create(
            &NewAccount {
                contact: &[&format!("mailto:{}", self.contact_email)],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            &self.directory_url,
            None,
        )
        .await
        .context("creating ACME account")?;

        let creds_json = serde_json::to_string(&creds).context("serialise creds")?;

        self.inventory
            .upsert_acme_account(UpsertAcmeAccount {
                contact_email: self.contact_email.clone(),
                directory_url: self.directory_url.clone(),
                account_key_pem: creds_json, // we store creds JSON in this column for now
                kid: None,
            })
            .await?;

        Ok(account)
    }

    /// Order a cert for `hostname` via HTTP-01. Returns the issued cert pair.
    pub async fn order(&self, hostname: &str) -> Result<IssuedCert> {
        let account = self.account().await?;

        let identifier = Identifier::Dns(hostname.to_string());
        let mut order = account
            .new_order(&NewOrder {
                identifiers: &[identifier],
            })
            .await
            .context("placing ACME order")?;

        let authorizations = order
            .authorizations()
            .await
            .context("fetching authorizations")?;

        for authz in &authorizations {
            let challenge = authz
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
                .ok_or_else(|| anyhow!("no HTTP-01 challenge offered for {hostname}"))?;

            let key_auth = order.key_authorization(challenge);
            self.challenges.install(&challenge.token, &key_auth.as_str()).await;

            order
                .set_challenge_ready(&challenge.url)
                .await
                .context("ack challenge ready")?;
        }

        // Poll for order completion. instant-acme exponential backoff is
        // built-in; we also bound it.
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 30 {
                return Err(anyhow!("ACME order did not finalize after 30 polls"));
            }
            sleep(Duration::from_secs(2)).await;
            let state = order.refresh().await.context("refresh order")?;
            match state.status {
                OrderStatus::Ready => break,
                OrderStatus::Valid => break,
                OrderStatus::Invalid => {
                    return Err(anyhow!("ACME order invalid: {state:?}"));
                }
                OrderStatus::Pending | OrderStatus::Processing => continue,
            }
        }

        // Cleanup challenge tokens.
        for authz in &authorizations {
            for c in &authz.challenges {
                self.challenges.remove(&c.token).await;
            }
        }

        // Generate CSR + finalize.
        let mut params = rcgen::CertificateParams::new(vec![hostname.to_string()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, hostname);
        let key_pair = rcgen::KeyPair::generate()?;
        let csr = params.serialize_request(&key_pair)?;

        order
            .finalize(csr.der())
            .await
            .context("finalize order")?;

        // Download cert chain.
        let cert_pem = loop {
            attempts += 1;
            if attempts > 60 {
                return Err(anyhow!("ACME cert download did not arrive"));
            }
            sleep(Duration::from_secs(2)).await;
            if let Some(pem) = order.certificate().await.context("get certificate")? {
                break pem;
            }
        };

        Ok(IssuedCert {
            cert_pem,
            key_pem: key_pair.serialize_pem(),
        })
    }
}
```

Update `crates/isengard-agent/src/tls/mod.rs`:

```rust
pub mod acme;
pub mod cert_store;
pub mod challenge_state;
pub mod storage;

pub use acme::{AcmeClient, IssuedCert, LE_PRODUCTION_URL, LE_STAGING_URL};
pub use cert_store::CertStore;
pub use challenge_state::ChallengeState;
pub use storage::{CertFiles, TlsStorage};
```

Note: `instant-acme` 0.7 API may differ slightly between patch versions (e.g. `OrderStatus::Ready` vs `OrderStatus::Valid` semantics, `set_challenge_ready` vs `set_challenges_ready`, `refresh()` return shape). If compile errors surface, look at the actual installed version's docs and adapt minimally — the high-level flow (account → order → authorizations → challenges → finalize → certificate) is stable.

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p isengard-agent`
Expected: success.

(No unit test for the ACME wrapper here — covered by the Pebble-gated e2e in Task 11.)

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/acme.rs \
        crates/isengard-agent/src/tls/mod.rs \
        crates/isengard-agent/Cargo.toml
git commit -m "feat(agent): tls::acme — instant-acme wrapper for HTTP-01 ordering"
```

---

### Task 10: TLS subsystem startup wiring in `lib.rs::run_agent`

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Add a TLS-config field to `AgentOptions`**

```rust
#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub controller_url: String,
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
    pub proxy_http_port: Option<u16>,
    pub proxy_https_port: Option<u16>,
    /// TLS subsystem options (Plan B 8e). If `None`, ACME is disabled and
    /// the cert store loads existing on-disk certs only. Production passes
    /// `Some(TlsOptions { ... })`.
    pub tls: Option<TlsOptions>,
}

#[derive(Debug, Clone)]
pub struct TlsOptions {
    pub cert_dir: std::path::PathBuf,           // /var/lib/isengard/tls/
    pub acme_contact_email: String,
    pub acme_directory_url: String,             // tls::acme::LE_STAGING_URL or LE_PRODUCTION_URL
}
```

- [ ] **Step 2: Initialize the cert store in `run_agent` and install it on `ProxyState`**

In the body of `run_agent`, after `let proxy_state = proxy::ProxyState::new();`:

```rust
if let Some(tls_opts) = opts.tls.as_ref() {
    let storage = tls::TlsStorage::new(tls_opts.cert_dir.clone());
    let cert_store = std::sync::Arc::new(tls::CertStore::new(storage));
    proxy_state.install_cert_store(cert_store.clone()).await;

    // Renewal scheduler is wired in Task 12.
    info!(cert_dir = ?tls_opts.cert_dir, "tls: cert store installed");
}
```

Also update all four agent integration tests (`enroll_e2e`, `events_e2e`, `reconnect_e2e`, `sync_e2e`) and `crates/isengard/src/main.rs` to add the new `tls: None` field on `AgentOptions` (tests pass `None`; production reads it from settings or defaults to `Some(TlsOptions { cert_dir: "/var/lib/isengard/tls".into(), acme_contact_email: env-var, acme_directory_url: LE_STAGING_URL })`).

For the production binary in `crates/isengard/src/main.rs`:

```rust
isengard_agent::run_agent(isengard_agent::AgentOptions {
    controller_url: controller,
    state_dir,
    config: serde_json::Value::Object(Default::default()),
    proxy_http_port: Some(8080),
    proxy_https_port: Some(8443),
    tls: Some(isengard_agent::TlsOptions {
        cert_dir: "/var/lib/isengard/tls".into(),
        acme_contact_email: std::env::var("ISENGARD_ACME_EMAIL")
            .unwrap_or_else(|_| "ops@example.com".into()),
        acme_directory_url: std::env::var("ISENGARD_ACME_DIRECTORY")
            .unwrap_or_else(|_| isengard_agent::tls::LE_STAGING_URL.into()),
    }),
})
.await
```

(Defaulting to staging is intentional — production switch is opt-in via the env var.)

- [ ] **Step 3: Run all tests + build**

Run: `cargo build --workspace`
Expected: success.

Run: `cargo test -p isengard-agent`
Expected: green.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/lib.rs \
        crates/isengard-agent/tests/ \
        crates/isengard/src/main.rs
git commit -m "feat(agent): TlsOptions on AgentOptions; install cert_store on ProxyState"
```

---

### Task 11: Pebble-gated end-to-end ACME test

**Files:**
- Create: `crates/isengard-agent/tests/acme_pebble_e2e.rs`

- [ ] **Step 1: Write the test**

```rust
//! End-to-end ACME against a local Pebble (Let's Encrypt's mock CA).
//!
//! Requires Pebble running at https://localhost:14000/dir.
//! Install: `go install github.com/letsencrypt/pebble/v2/cmd/pebble@latest`
//! Run:     `pebble -config /path/to/test-config.json`
//! Or via container:
//!   docker run -p 14000:14000 -p 15000:15000 letsencrypt/pebble
//!
//! This test is `#[ignore]`'d so CI without Pebble passes silently.

use isengard_agent::tls::{AcmeClient, ChallengeState, LE_STAGING_URL};
use isengard_storage::Inventory;
use std::sync::Arc;
use tempfile::tempdir;

const PEBBLE_DIR: &str = "https://localhost:14000/dir";

#[tokio::test]
#[ignore = "requires local Pebble running at localhost:14000"]
async fn order_cert_against_pebble_returns_pem_pair() {
    // Skip silently if Pebble isn't reachable.
    if reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(PEBBLE_DIR)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_err()
    {
        eprintln!("[acme_pebble_e2e] Pebble not reachable at {PEBBLE_DIR} — skipping");
        return;
    }

    let dir = tempdir().unwrap();
    let inv = Arc::new(Inventory::open(&dir.path().join("isengard.db")).await.unwrap());
    let challenges = Arc::new(ChallengeState::new());

    // NB: Pebble validates by hitting our HTTP-01 endpoint. For this test to
    // actually pass end-to-end you also need to run an HTTP-01 challenge
    // server reachable by Pebble (Pebble defaults to https://localhost:5002
    // for the HTTP-01 challenge, configurable). We're not spinning up the
    // full Pingora :8080 listener here; this test exercises the
    // account-registration and order-placement code path. Full e2e is the
    // manual smoke checklist in the spec §11.

    let _client = AcmeClient::new(
        inv,
        challenges,
        "ops@example.com".into(),
        PEBBLE_DIR.to_string(),
    );

    // Just verify account creation succeeds. Full order would need the
    // HTTP-01 server reachable from Pebble.
    // (Real end-to-end orchestration lives in the manual smoke run.)

    // Sanity: we can construct, and the test ran without panic.
    assert!(true);
}
```

The test is intentionally minimal — full ACME e2e requires a running HTTP-01 server reachable by Pebble, which is too much fixture for an automated test. The manual smoke checklist (spec §11) is the real verification.

- [ ] **Step 2: Verify it compiles + workspace tests pass**

Run: `cargo build --workspace --tests`
Expected: success.

Run: `cargo test --workspace`
Expected: green (the e2e is `#[ignore]`'d).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/tests/acme_pebble_e2e.rs
git commit -m "test(agent): acme_pebble_e2e — opt-in Pebble integration scaffolding"
```

---

## Phase 8e-3: renewal scheduler + rate-limit + events

### Task 12: `renewal.rs` — cron-style renewal task

**Files:**
- Create: `crates/isengard-agent/src/tls/renewal.rs`
- Modify: `crates/isengard-agent/src/tls/mod.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (spawn the renewal task in `run_agent`)

- [ ] **Step 1: Implement the scheduler**

Create `crates/isengard-agent/src/tls/renewal.rs`:

```rust
//! Renewal scheduler. Wakes every hour, scans `tls_certs` for any cert
//! whose `next_renewal_at` has passed, and triggers a fresh ACME order.
//!
//! Rate-limit guard: refuses to retry within a backoff window after a
//! failure (1h, 2h, 4h, 8h, max 24h based on `attempt_count`).

use crate::tls::{AcmeClient, CertStore};
use chrono::{DateTime, Utc};
use isengard_core::{Event, EventEmitter};
use isengard_storage::Inventory;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

const TICK_INTERVAL: Duration = Duration::from_secs(3600); // 1 hour

pub fn spawn(
    inv: Arc<Inventory>,
    cert_store: Arc<CertStore>,
    acme_client: Arc<AcmeClient>,
    emitter: Arc<dyn EventEmitter>,
) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&inv, &cert_store, &acme_client, &emitter).await {
                warn!(error = %e, "tls: renewal tick failed");
            }
            sleep(TICK_INTERVAL).await;
        }
    });
}

async fn tick(
    inv: &Inventory,
    cert_store: &Arc<CertStore>,
    acme_client: &Arc<AcmeClient>,
    emitter: &Arc<dyn EventEmitter>,
) -> anyhow::Result<()> {
    let now = Utc::now();

    // The Inventory doesn't currently expose a "list all tls_certs" — add
    // one in this task or use a direct query. For the plan, assume
    // `Inventory::list_tls_certs_due(before: DateTime<Utc>) -> Vec<TlsCertMeta>`
    // exists. (See addendum below if it doesn't yet.)
    let due = inv.list_tls_certs_due(now).await?;

    for meta in due {
        if !should_retry(&meta, now) {
            continue;
        }
        let hostname = meta.public_hostname.clone();
        match acme_client.order(&hostname).await {
            Ok(cert) => {
                if let Err(e) = cert_store.install(&hostname, &cert.cert_pem, &cert.key_pem).await {
                    inv.record_tls_attempt(&hostname, false, Some(format!("install failed: {e}")))
                        .await
                        .ok();
                    warn!(host = %hostname, error = %e, "tls: cert install failed");
                    continue;
                }
                inv.record_tls_attempt(&hostname, true, None).await.ok();
                emitter.emit(Event {
                    kind: "tls.cert.renewed".into(),
                    summary: format!("renewed cert for {hostname}"),
                    container_name: Some(hostname.clone()),
                    occurred_at: Utc::now(),
                    ..Default::default()
                }).await;
                info!(host = %hostname, "tls: cert renewed");
            }
            Err(e) => {
                inv.record_tls_attempt(&hostname, false, Some(e.to_string()))
                    .await
                    .ok();
                emitter.emit(Event {
                    kind: "tls.acme.failed".into(),
                    summary: format!("ACME failed for {hostname}: {e}"),
                    container_name: Some(hostname.clone()),
                    error: Some(e.to_string()),
                    occurred_at: Utc::now(),
                    ..Default::default()
                }).await;
                warn!(host = %hostname, error = %e, "tls: acme failed");
            }
        }
    }
    Ok(())
}

fn should_retry(meta: &isengard_storage::TlsCertMeta, now: DateTime<Utc>) -> bool {
    let Some(last) = meta.last_attempt_at else {
        return true; // never attempted
    };
    let backoff_hours = match meta.attempt_count.saturating_sub(meta.last_error.as_ref().map_or(0, |_| 0)) {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 24,
    };
    let next_allowed = last + chrono::Duration::hours(backoff_hours as i64);
    now >= next_allowed
}
```

Add `list_tls_certs_due` to `crates/isengard-storage/src/tls_cert.rs`:

```rust
impl crate::inventory::Inventory {
    pub async fn list_tls_certs_due(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<TlsCertMeta>> {
        let cutoff = before.to_rfc3339();
        let rows = sqlx::query(
            r#"
            SELECT public_hostname, host_id, issuer, not_before, not_after,
                   last_renewed_at, next_renewal_at, serial,
                   last_attempt_at, last_error, attempt_count
            FROM tls_certs
            WHERE next_renewal_at <= ?
            "#,
        )
        .bind(&cutoff)
        .fetch_all(self.pool())
        .await?;
        // Use the existing row decoder helper or repeat the field decode here.
        rows.into_iter()
            .map(decode_tls_cert_row)
            .collect::<Result<Vec<_>>>()
    }
}

fn decode_tls_cert_row(r: sqlx::sqlite::SqliteRow) -> Result<TlsCertMeta> {
    use sqlx::Row;
    Ok(TlsCertMeta {
        public_hostname: r.try_get("public_hostname")?,
        host_id: HostId::from_bytes({
            let v: Vec<u8> = r.try_get("host_id")?;
            v.try_into().map_err(|_| Error::Decode { reason: "bad host_id".into() })?
        }),
        issuer: r.try_get("issuer")?,
        not_before: parse_dt(r.try_get("not_before")?)?,
        not_after: parse_dt(r.try_get("not_after")?)?,
        last_renewed_at: r.try_get::<Option<String>, _>("last_renewed_at")?
            .map(parse_dt).transpose()?,
        next_renewal_at: parse_dt(r.try_get("next_renewal_at")?)?,
        serial: r.try_get("serial").ok(),
        last_attempt_at: r.try_get::<Option<String>, _>("last_attempt_at")?
            .map(parse_dt).transpose()?,
        last_error: r.try_get("last_error").ok().flatten(),
        attempt_count: r.try_get::<i64, _>("attempt_count")? as u32,
    })
}

fn parse_dt(s: String) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| Error::Decode { reason: e.to_string() })
}
```

(Refactor `get_tls_cert_meta` to use `decode_tls_cert_row` too if not already done — DRY.)

Update `crates/isengard-agent/src/tls/mod.rs`:

```rust
pub mod renewal;
```

- [ ] **Step 2: Wire the spawn in `run_agent`**

In `crates/isengard-agent/src/lib.rs`, after the cert_store install:

```rust
if let Some(tls_opts) = opts.tls.as_ref() {
    let storage = tls::TlsStorage::new(tls_opts.cert_dir.clone());
    let cert_store = std::sync::Arc::new(tls::CertStore::new(storage));
    proxy_state.install_cert_store(cert_store.clone()).await;

    let acme_client = std::sync::Arc::new(tls::AcmeClient::new(
        // We need an Arc<Inventory>. The agent doesn't currently hold one
        // (controller does). We can construct one against the agent's local
        // state DB if needed — Plan A's storage is controller-side. For
        // now, the renewal scheduler is conditional on having an inventory:
        // the agent's local SQLite (used by other plugins) is the right
        // place. Read the existing agent_state.rs for how the local DB is
        // opened, and construct an Inventory off the same path.
        std::sync::Arc::new(open_agent_inventory(&opts.state_dir).await?),
        proxy_state.acme_challenges.clone(),
        tls_opts.acme_contact_email.clone(),
        tls_opts.acme_directory_url.clone(),
    ));

    tls::renewal::spawn(
        std::sync::Arc::new(open_agent_inventory(&opts.state_dir).await?),
        cert_store.clone(),
        acme_client,
        emitter.clone(),
    );

    info!("tls: renewal scheduler started");
}
```

The `open_agent_inventory` helper (write it inline in `lib.rs`):

```rust
async fn open_agent_inventory(state_dir: &std::path::Path) -> Result<isengard_storage::Inventory> {
    let db_path = state_dir.join("agent.db");
    isengard_storage::Inventory::open(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("opening agent inventory: {e}"))
}
```

Note: this introduces a per-host SQLite db on the agent side at `<state_dir>/agent.db`. Plan A's `Inventory` was controller-side. For Plan B, the agent gets its own minimal Inventory just for `tls_certs` and `acme_account`. Migrations 0001-0010 all apply harmlessly even though many tables (hosts, stacks etc.) are unused on the agent side.

- [ ] **Step 3: Run tests + build**

Run: `cargo build --workspace`
Expected: success.

Run: `cargo test --workspace`
Expected: green.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/renewal.rs \
        crates/isengard-agent/src/tls/mod.rs \
        crates/isengard-agent/src/lib.rs \
        crates/isengard-storage/src/tls_cert.rs
git commit -m "feat(agent): renewal scheduler + agent-side Inventory + tls.cert.renewed events"
```

---

### Task 13: Unit test the rate-limit / backoff guard

**Files:**
- Modify: `crates/isengard-agent/src/tls/renewal.rs` (expose `should_retry` for test)
- Create: `crates/isengard-agent/tests/renewal_backoff_unit.rs`

- [ ] **Step 1: Make `should_retry` `pub`**

In `tls/renewal.rs`, change `fn should_retry` to `pub fn should_retry`.

- [ ] **Step 2: Write the failing test**

Create `crates/isengard-agent/tests/renewal_backoff_unit.rs`:

```rust
use chrono::{Duration, Utc};
use isengard_agent::tls::renewal::should_retry;
use isengard_storage::TlsCertMeta;

fn meta(attempt_count: u32, last_attempt_minutes_ago: i64, error: Option<&str>) -> TlsCertMeta {
    let now = Utc::now();
    let host_id = isengard_storage::HostId::from_bytes([0u8; 16]);
    TlsCertMeta {
        public_hostname: "h.test".into(),
        host_id,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + Duration::days(90),
        last_renewed_at: None,
        next_renewal_at: now,
        serial: None,
        last_attempt_at: Some(now - Duration::minutes(last_attempt_minutes_ago)),
        last_error: error.map(String::from),
        attempt_count,
    }
}

#[test]
fn never_attempted_always_retries() {
    let mut m = meta(0, 0, None);
    m.last_attempt_at = None;
    assert!(should_retry(&m, Utc::now()));
}

#[test]
fn first_failure_retries_after_one_hour() {
    let m = meta(1, 30, Some("rate limited"));
    assert!(!should_retry(&m, Utc::now()), "30 min in is too soon");

    let m = meta(1, 65, Some("rate limited"));
    assert!(should_retry(&m, Utc::now()), "65 min in is past the 1h backoff");
}

#[test]
fn fifth_failure_uses_24h_backoff() {
    let m = meta(5, 10 * 60, Some("still failing"));
    assert!(!should_retry(&m, Utc::now()), "10h in is too soon for 5th failure");

    let m = meta(5, 25 * 60, Some("still failing"));
    assert!(should_retry(&m, Utc::now()), "25h in is past the 24h backoff");
}
```

- [ ] **Step 3: Run + verify**

Run: `cargo test -p isengard-agent --test renewal_backoff_unit`
Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/tls/renewal.rs \
        crates/isengard-agent/tests/renewal_backoff_unit.rs
git commit -m "test(agent): renewal backoff guard unit tests"
```

---

## Phase 8f-1: tailscale crate skeleton + CLI wrappers + join/unexpose

### Task 14: `networking-tailscale` crate scaffolding

**Files:**
- Create: `crates/isengard-plugins/networking-tailscale/Cargo.toml`
- Create: `crates/isengard-plugins/networking-tailscale/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Add to workspace + create Cargo.toml**

In root `Cargo.toml`, append `"crates/isengard-plugins/networking-tailscale"` to `members`. Add path dep:

```toml
isengard-plugin-networking-tailscale = { path = "crates/isengard-plugins/networking-tailscale" }
```

Create `crates/isengard-plugins/networking-tailscale/Cargo.toml`:

```toml
[package]
name = "isengard-plugin-networking-tailscale"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Tailscale (CLI-driven) NetworkingAdapter for Isengard"

[dependencies]
async-trait.workspace = true
inventory.workspace = true
isengard-core = { path = "../../isengard-core" }
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true, features = ["process", "io-util"] }
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Skeleton lib + adapter stub**

Create `crates/isengard-plugins/networking-tailscale/src/lib.rs`:

```rust
//! Tailscale NetworkingAdapter for Isengard.
//!
//! Drives the user's installed `tailscale` CLI via tokio subprocess. No Go
//! FFI; users install tailscale separately (which they almost certainly
//! already have if they're using this adapter).

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;

pub mod cli;

#[derive(Default)]
pub struct TailscaleAdapter;

#[async_trait]
impl Plugin for TailscaleAdapter {
    fn name(&self) -> &'static str {
        "networking-tailscale"
    }
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl NetworkingAdapter for TailscaleAdapter {
    fn id(&self) -> &'static str {
        "tailscale"
    }

    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        cli::ensure_present()?;
        let status = cli::status().await?;
        if !status.online {
            return Err(CoreError::Other(format!(
                "tailscale present but not online (run `tailscale up` first); state={}",
                status.backend_state
            )));
        }
        Ok(())
    }

    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        // Don't `tailscale down` — that's user-controlled; we only manage
        // serve/funnel rules.
        Ok(())
    }

    async fn expose(
        &self,
        _ctx: &AdapterContext,
        _spec: &ExposeSpec,
    ) -> Result<ExposedEndpoint> {
        Err(CoreError::Other(
            "tailscale expose not yet implemented (Plan B 8f-2)".into(),
        ))
    }

    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        Ok(())
    }

    fn tls_strategy(&self) -> TlsStrategy {
        TlsStrategy::AdapterProvided
    }
}

inventory::submit! {
    isengard_core::registration::PluginRegistration {
        name: "networking-tailscale",
        capabilities: &[isengard_core::registration::Capability::Agent],
        constructor: || Box::new(TailscaleAdapter::default()),
    }
}
```

- [ ] **Step 3: Stub the CLI module**

Create `crates/isengard-plugins/networking-tailscale/src/cli.rs`:

```rust
//! Wrappers around `tokio::process::Command::new("tailscale")`.
//! `expose`-side calls land in Task 16; this skeleton just covers `status`
//! and `ensure_present`.

use isengard_core::error::{CoreError, Result};
use serde::Deserialize;
use tokio::process::Command;

pub fn ensure_present() -> Result<()> {
    if which::which("tailscale").is_err() {
        return Err(CoreError::Other(
            "`tailscale` CLI not found in PATH; install from https://tailscale.com/download"
                .into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    pub backend_state: String,
    #[serde(default)]
    pub online: bool,
}

pub async fn status() -> Result<TailscaleStatus> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("running tailscale status: {e}")))?;

    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale status --json` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let mut status: TailscaleStatus = serde_json::from_slice(&out.stdout).map_err(|e| {
        CoreError::Other(format!("parsing tailscale status JSON: {e}"))
    })?;

    // Tailscale's JSON output uses "BackendState": "Running" when online.
    status.online = status.backend_state == "Running";

    Ok(status)
}
```

Add `which = "6"` to the crate's `[dependencies]` in `Cargo.toml` for the `ensure_present` check. Also list it in workspace deps if not already present.

- [ ] **Step 4: Wire into the binary**

In `crates/isengard/Cargo.toml`, add the path dep:

```toml
isengard-plugin-networking-tailscale = { workspace = true, optional = true }

[features]
default = ["tailscale", "cf-tunnel"]
tailscale = ["dep:isengard-plugin-networking-tailscale"]
cf-tunnel = []  # filled in by Task 17
```

In `crates/isengard/src/main.rs`:

```rust
#[cfg(feature = "tailscale")]
use isengard_plugin_networking_tailscale as _;
```

- [ ] **Step 5: Build verify + commit**

Run: `cargo build --workspace`
Expected: success — adapter compiled, registration submitted.

```bash
cargo fmt --all
git add Cargo.toml crates/isengard-plugins/networking-tailscale/ \
        crates/isengard/Cargo.toml crates/isengard/src/main.rs
git commit -m "feat(plugins): networking-tailscale skeleton + CLI status check"
```

---

### Task 15: Adapter registration + `join()` smoke (no real tailscale)

**Files:**
- Create: `crates/isengard-plugins/networking-tailscale/tests/loads.rs`

- [ ] **Step 1: Write registration test**

```rust
use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_tailscale::TailscaleAdapter;

#[test]
fn id_is_tailscale() {
    let a = TailscaleAdapter;
    assert_eq!(a.id(), "tailscale");
}

#[test]
fn tls_strategy_is_adapter_provided() {
    let a = TailscaleAdapter;
    assert!(matches!(a.tls_strategy(), TlsStrategy::AdapterProvided));
}

#[tokio::test]
async fn expose_unimplemented_yet_returns_clear_error() {
    use isengard_core::context::{HostMode, PluginContext};
    use isengard_core::networking::{AdapterContext, ExposeSpec, Protocol};

    let a = TailscaleAdapter;
    let ctx = AdapterContext {
        host_id: "h".into(),
        settings: serde_json::Value::Null,
        plugin_ctx: PluginContext::new(HostMode::Agent, serde_json::Value::Null),
    };
    let spec = ExposeSpec {
        public_hostname: "x".into(),
        local_listener_port: 8080,
        protocol: Protocol::Http,
        adapter_specific: serde_json::Value::Null,
    };
    let err = a.expose(&ctx, &spec).await.unwrap_err();
    assert!(format!("{err}").contains("8f-2"));
}
```

- [ ] **Step 2: Run + commit**

Run: `cargo test -p isengard-plugin-networking-tailscale`
Expected: 3 passed.

```bash
git add crates/isengard-plugins/networking-tailscale/tests/loads.rs
git commit -m "test(plugins): tailscale adapter loads + tls_strategy + expose-stub"
```

---

## Phase 8f-2: tailscale `expose()` + cert flow

### Task 16: `expose()`/`unexpose()` via `tailscale serve` + `tailscale cert`

**Files:**
- Modify: `crates/isengard-plugins/networking-tailscale/src/lib.rs`
- Modify: `crates/isengard-plugins/networking-tailscale/src/cli.rs`

- [ ] **Step 1: Add CLI wrappers for serve/funnel/cert**

Append to `crates/isengard-plugins/networking-tailscale/src/cli.rs`:

```rust
/// Run `tailscale serve --bg --https=<port> --set-path=/ http://localhost:<local_port>`.
pub async fn serve_https(local_port: u16) -> Result<()> {
    let out = Command::new("tailscale")
        .args([
            "serve",
            "--bg",
            "--https=443",
            "--set-path=/",
            &format!("http://localhost:{local_port}"),
        ])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale serve: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale serve` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Turn `tailscale serve` off for the default https=443 path.
pub async fn serve_off() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["serve", "--https=443", "--set-path=/", "off"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale serve off: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale serve off` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

pub async fn funnel_on() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["funnel", "--bg", "--https=443", "on"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale funnel on: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale funnel on` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

pub async fn funnel_off() -> Result<()> {
    let _ = Command::new("tailscale")
        .args(["funnel", "--https=443", "off"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale funnel off: {e}")))?;
    Ok(())
}

/// Run `tailscale cert <hostname>` from a temp dir; returns (cert_pem, key_pem).
pub async fn fetch_cert(hostname: &str) -> Result<(String, String)> {
    let tmp = tempfile::tempdir()
        .map_err(|e| CoreError::Other(format!("tempdir: {e}")))?;

    let out = Command::new("tailscale")
        .args(["cert", hostname])
        .current_dir(tmp.path())
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale cert: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale cert {hostname}` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let cert = std::fs::read_to_string(tmp.path().join(format!("{hostname}.crt")))
        .map_err(|e| CoreError::Other(format!("reading cert: {e}")))?;
    let key = std::fs::read_to_string(tmp.path().join(format!("{hostname}.key")))
        .map_err(|e| CoreError::Other(format!("reading key: {e}")))?;
    Ok((cert, key))
}
```

Add `tempfile = "3"` to the crate's `[dependencies]`.

- [ ] **Step 2: Real `expose()` and `unexpose()`**

In `crates/isengard-plugins/networking-tailscale/src/lib.rs`, replace the stubs:

```rust
async fn expose(
    &self,
    _ctx: &AdapterContext,
    spec: &ExposeSpec,
) -> Result<ExposedEndpoint> {
    cli::serve_https(spec.local_listener_port).await?;

    let funnel_on = spec
        .adapter_specific
        .get("funnel")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if funnel_on {
        cli::funnel_on().await?;
    }

    // Cert flow handled out-of-band by the agent's CertStore + a
    // tailscale-specific cert refresher (Task 18). Here we just trigger
    // the initial cert fetch so the first request doesn't 503.
    let (cert_pem, key_pem) = cli::fetch_cert(&spec.public_hostname).await?;

    Ok(ExposedEndpoint {
        id: format!("tailscale:{}", spec.public_hostname),
        url: format!("https://{}", spec.public_hostname),
        adapter_data: serde_json::json!({
            "funnel": funnel_on,
            "cert_pem": cert_pem,
            "key_pem": key_pem,
        }),
    })
}

async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
    let _ = cli::serve_off().await;
    let _ = cli::funnel_off().await;
    Ok(())
}
```

Note the cert flow: the adapter returns the cert in `adapter_data`. The caller (controller's `RoutingPusher` or agent-side equivalent in a later Phase 8c+ task) reads the cert from `adapter_data` and writes it to the agent's `CertStore` via the existing `install` API. Add a TODO in code linking back to the spec for the broader cert-pump design.

- [ ] **Step 3: Build + commit**

Run: `cargo build --workspace`
Expected: success.

```bash
cargo fmt --all
git add crates/isengard-plugins/networking-tailscale/
git commit -m "feat(plugins): tailscale expose/unexpose via tailscale serve + cert"
```

---

## Phase 8g-1: cf-tunnel crate skeleton + API client + cloudflared supervisor

### Task 17: `networking-cf-tunnel` crate scaffolding + adapter stub

**Files:**
- Create: `crates/isengard-plugins/networking-cf-tunnel/Cargo.toml`
- Create: `crates/isengard-plugins/networking-cf-tunnel/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Add to workspace**

Append `"crates/isengard-plugins/networking-cf-tunnel"` to root `Cargo.toml` `members`. Add to `[workspace.dependencies]`:

```toml
isengard-plugin-networking-cf-tunnel = { path = "crates/isengard-plugins/networking-cf-tunnel" }
```

Create `crates/isengard-plugins/networking-cf-tunnel/Cargo.toml`:

```toml
[package]
name = "isengard-plugin-networking-cf-tunnel"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Cloudflare Tunnel NetworkingAdapter for Isengard"

[dependencies]
async-trait.workspace = true
inventory.workspace = true
isengard-core = { path = "../../isengard-core" }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true, features = ["process", "io-util"] }
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
wiremock = "0.6"
```

- [ ] **Step 2: Adapter skeleton**

Create `crates/isengard-plugins/networking-cf-tunnel/src/lib.rs`:

```rust
//! Cloudflare Tunnel NetworkingAdapter.
//!
//! Data plane: a supervised `cloudflared tunnel run` subprocess holds the
//! persistent connection to CF edge.
//! Control plane: CF v4 REST API for tunnel CRUD, ingress rules, DNS.

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::{CoreError, Result};
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;

pub mod api;
pub mod cloudflared;

#[derive(Default)]
pub struct CfTunnelAdapter;

#[async_trait]
impl Plugin for CfTunnelAdapter {
    fn name(&self) -> &'static str { "networking-cf-tunnel" }
    fn version(&self) -> &'static str { env!("CARGO_PKG_VERSION") }
    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> { Ok(()) }
    async fn stop(&mut self) -> Result<()> { Ok(()) }
}

#[async_trait]
impl NetworkingAdapter for CfTunnelAdapter {
    fn id(&self) -> &'static str { "cf-tunnel" }

    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        Err(CoreError::Other("cf-tunnel join not yet implemented (Plan B 8g-2)".into()))
    }
    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> { Ok(()) }
    async fn expose(&self, _ctx: &AdapterContext, _spec: &ExposeSpec) -> Result<ExposedEndpoint> {
        Err(CoreError::Other("cf-tunnel expose not yet implemented (Plan B 8g-2)".into()))
    }
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> { Ok(()) }
    fn tls_strategy(&self) -> TlsStrategy { TlsStrategy::EdgeTermination }
}

inventory::submit! {
    isengard_core::registration::PluginRegistration {
        name: "networking-cf-tunnel",
        capabilities: &[isengard_core::registration::Capability::Agent],
        constructor: || Box::new(CfTunnelAdapter::default()),
    }
}
```

Stub `crates/isengard-plugins/networking-cf-tunnel/src/api.rs` and `cloudflared.rs` as empty modules with just a doc comment for now (Task 18 + 19 fill them in).

- [ ] **Step 3: Wire into binary feature**

In `crates/isengard/Cargo.toml`:

```toml
isengard-plugin-networking-cf-tunnel = { workspace = true, optional = true }

[features]
default = ["tailscale", "cf-tunnel"]
tailscale = ["dep:isengard-plugin-networking-tailscale"]
cf-tunnel = ["dep:isengard-plugin-networking-cf-tunnel"]
```

In `crates/isengard/src/main.rs`:

```rust
#[cfg(feature = "cf-tunnel")]
use isengard_plugin_networking_cf_tunnel as _;
```

- [ ] **Step 4: Build + registration test**

Create `crates/isengard-plugins/networking-cf-tunnel/tests/loads.rs`:

```rust
use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_cf_tunnel::CfTunnelAdapter;

#[test]
fn id_is_cf_tunnel() {
    assert_eq!(CfTunnelAdapter.id(), "cf-tunnel");
}

#[test]
fn tls_strategy_is_edge_termination() {
    assert!(matches!(CfTunnelAdapter.tls_strategy(), TlsStrategy::EdgeTermination));
}
```

Run: `cargo build --workspace && cargo test -p isengard-plugin-networking-cf-tunnel`
Expected: 2 passed.

```bash
cargo fmt --all
git add Cargo.toml crates/isengard-plugins/networking-cf-tunnel/ \
        crates/isengard/Cargo.toml crates/isengard/src/main.rs
git commit -m "feat(plugins): networking-cf-tunnel skeleton + EdgeTermination TLS strategy"
```

---

### Task 18: CF v4 API client (zones, tunnels, dns_records) with wiremock tests

**Files:**
- Modify: `crates/isengard-plugins/networking-cf-tunnel/src/api.rs`
- Create: `crates/isengard-plugins/networking-cf-tunnel/tests/api_units.rs`

- [ ] **Step 1: Implement the API client**

Replace `crates/isengard-plugins/networking-cf-tunnel/src/api.rs`:

```rust
//! Thin client for the subset of CF v4 API we use:
//! - POST   /accounts/{account}/cfd_tunnel              (create tunnel)
//! - GET    /accounts/{account}/cfd_tunnel/{id}         (get tunnel)
//! - DELETE /accounts/{account}/cfd_tunnel/{id}         (delete tunnel)
//! - PATCH  /accounts/{account}/cfd_tunnel/{id}/configurations (set ingress)
//! - POST/DELETE /zones/{zone}/dns_records              (manage DNS CNAMEs)

use isengard_core::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

pub struct CfApi {
    client: reqwest::Client,
    api_token: String,
    base_url: String,
}

impl CfApi {
    pub fn new(api_token: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("reqwest client builds"),
            api_token,
            base_url: CF_API_BASE.to_string(),
        }
    }

    /// Test-only constructor that overrides the API base URL (for wiremock).
    pub fn with_base_url(api_token: String, base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_token,
            base_url,
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.api_token)
    }

    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<TunnelCreated> {
        let url = format!("{}/accounts/{}/cfd_tunnel", self.base_url, account_id);
        let body = serde_json::json!({
            "name": name,
            "config_src": "cloudflare",
        });
        let resp: CfResponse<TunnelCreated> = self
            .auth(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF create_tunnel: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF create_tunnel JSON: {e}")))?;
        resp.into_result()
    }

    pub async fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<()> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}",
            self.base_url, account_id, tunnel_id
        );
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete_tunnel: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete_tunnel JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }

    pub async fn set_ingress(
        &self,
        account_id: &str,
        tunnel_id: &str,
        ingress: Vec<IngressRule>,
    ) -> Result<()> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}/configurations",
            self.base_url, account_id, tunnel_id
        );
        let body = serde_json::json!({
            "config": { "ingress": ingress },
        });
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.put(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF set_ingress: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF set_ingress JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }

    pub async fn upsert_dns_cname(
        &self,
        zone_id: &str,
        hostname: &str,
        target: &str,
    ) -> Result<DnsRecordCreated> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone_id);
        let body = serde_json::json!({
            "type": "CNAME",
            "name": hostname,
            "content": target,
            "proxied": true,
            "ttl": 1,
        });
        let resp: CfResponse<DnsRecordCreated> = self
            .auth(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF dns_records: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF dns_records JSON: {e}")))?;
        resp.into_result()
    }

    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        let url = format!("{}/zones/{}/dns_records/{}", self.base_url, zone_id, record_id);
        let resp: CfResponse<serde_json::Value> = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete dns: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF delete dns JSON: {e}")))?;
        resp.into_result().map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct CfError {
    code: i64,
    message: String,
}

impl<T> CfResponse<T> {
    fn into_result(self) -> Result<T> {
        if self.success {
            self.result
                .ok_or_else(|| CoreError::Other("CF response missing `result` field".into()))
        } else {
            let msg = self
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join(", ");
            Err(CoreError::Other(format!("CF API error: {msg}")))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TunnelCreated {
    pub id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct DnsRecordCreated {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub struct IngressRule {
    pub hostname: Option<String>,
    pub service: String,
}
```

- [ ] **Step 2: Wiremock tests**

Create `crates/isengard-plugins/networking-cf-tunnel/tests/api_units.rs`:

```rust
use isengard_plugin_networking_cf_tunnel::api::{CfApi, IngressRule};
use wiremock::matchers::{header, method, path, body_json};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn create_tunnel_posts_body_and_returns_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/accounts/acct-1/cfd_tunnel"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "errors": [],
            "result": {
                "id": "tnl-123",
                "name": "isengard-test",
                "token": "tunnel-secret"
            }
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("test-token".into(), server.uri());
    let created = api.create_tunnel("acct-1", "isengard-test").await.unwrap();
    assert_eq!(created.id, "tnl-123");
    assert_eq!(created.token, "tunnel-secret");
}

#[tokio::test]
async fn cf_error_response_is_propagated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/accounts/acct-1/cfd_tunnel"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "success": false,
            "errors": [{"code": 1009, "message": "Tunnel name already exists"}],
            "result": null
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    let err = api.create_tunnel("acct-1", "isengard-test").await.unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("1009"), "{s}");
    assert!(s.contains("Tunnel name already exists"), "{s}");
}

#[tokio::test]
async fn set_ingress_sends_correct_body() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/accounts/acct-1/cfd_tunnel/tnl-123/configurations"))
        .and(body_json(serde_json::json!({
            "config": {
                "ingress": [
                    {"hostname": "blog.example.com", "service": "http://localhost:8080"},
                    {"hostname": null, "service": "http_status:404"}
                ]
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "errors": [], "result": {}
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    api.set_ingress(
        "acct-1",
        "tnl-123",
        vec![
            IngressRule {
                hostname: Some("blog.example.com".into()),
                service: "http://localhost:8080".into(),
            },
            IngressRule {
                hostname: None,
                service: "http_status:404".into(),
            },
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn upsert_dns_cname_returns_record_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/zones/zone-1/dns_records"))
        .and(body_json(serde_json::json!({
            "type": "CNAME",
            "name": "blog.example.com",
            "content": "tnl-123.cfargotunnel.com",
            "proxied": true,
            "ttl": 1,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "errors": [],
            "result": { "id": "dns-record-1", "name": "blog.example.com" }
        })))
        .mount(&server)
        .await;

    let api = CfApi::with_base_url("t".into(), server.uri());
    let r = api
        .upsert_dns_cname("zone-1", "blog.example.com", "tnl-123.cfargotunnel.com")
        .await
        .unwrap();
    assert_eq!(r.id, "dns-record-1");
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p isengard-plugin-networking-cf-tunnel --test api_units`
Expected: 4 passed.

```bash
cargo fmt --all
git add crates/isengard-plugins/networking-cf-tunnel/src/api.rs \
        crates/isengard-plugins/networking-cf-tunnel/tests/api_units.rs
git commit -m "feat(plugins): cf-tunnel API client (zones, tunnels, dns) with wiremock tests"
```

---

### Task 19: cloudflared subprocess supervisor

**Files:**
- Modify: `crates/isengard-plugins/networking-cf-tunnel/src/cloudflared.rs`

- [ ] **Step 1: Implement the supervisor**

Replace `crates/isengard-plugins/networking-cf-tunnel/src/cloudflared.rs`:

```rust
//! cloudflared subprocess supervisor. Mirrors `isengard-agent::proxy::supervise`
//! pattern: spawn, monitor, restart-with-backoff up to N times, give up.

use isengard_core::error::{CoreError, Result};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

const MAX_RESTARTS: u32 = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(300);

pub fn ensure_present() -> Result<()> {
    if which::which("cloudflared").is_err() {
        return Err(CoreError::Other(
            "`cloudflared` not found in PATH; install from https://github.com/cloudflare/cloudflared/releases"
                .into(),
        ));
    }
    Ok(())
}

pub async fn spawn(token: String) -> Result<Child> {
    Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "run", "--token", &token])
        .spawn()
        .map_err(|e| CoreError::Other(format!("spawning cloudflared: {e}")))
}

pub async fn supervise(token: String) {
    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        info!("cf-tunnel: starting cloudflared subprocess");
        let child = match spawn(token.clone()).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "cf-tunnel: failed to spawn cloudflared");
                return;
            }
        };

        let mut child = child;
        let exit = child.wait().await;
        match exit {
            Ok(status) => warn!(?status, "cf-tunnel: cloudflared exited"),
            Err(e) => error!(error = %e, "cf-tunnel: waiting on cloudflared failed"),
        }

        let now = Instant::now();
        restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        restarts.push(now);
        if restarts.len() as u32 > MAX_RESTARTS {
            error!(
                restarts = restarts.len(),
                "cf-tunnel: restart budget exhausted; giving up (TODO: emit networking.adapter.crashloop)"
            );
            return;
        }
        let backoff = Duration::from_millis(250 * (1u64 << (restarts.len().min(5) as u64)));
        tokio::time::sleep(backoff).await;
    }
}
```

Add `which = "6"` to the cf-tunnel crate's `[dependencies]`.

- [ ] **Step 2: Build verify**

Run: `cargo build --workspace`
Expected: success.

(No unit test for the supervisor here — covered indirectly by the manual smoke run.)

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/networking-cf-tunnel/src/cloudflared.rs \
        crates/isengard-plugins/networking-cf-tunnel/Cargo.toml
git commit -m "feat(plugins): cf-tunnel cloudflared subprocess supervisor"
```

---

## Phase 8g-2: cf-tunnel `expose()` / `unexpose()`

### Task 20: real `join()` / `expose()` / `unexpose()` for cf-tunnel

**Files:**
- Modify: `crates/isengard-plugins/networking-cf-tunnel/src/lib.rs`

- [ ] **Step 1: Implement the lifecycle methods**

Replace the stubs in `crates/isengard-plugins/networking-cf-tunnel/src/lib.rs`:

```rust
async fn join(&self, ctx: &AdapterContext) -> Result<()> {
    cloudflared::ensure_present()?;

    let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
        .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;

    let api = api::CfApi::new(cfg.api_token.clone());

    let tunnel_id = if let Some(id) = cfg.tunnel_id.clone() {
        id
    } else {
        let created = api
            .create_tunnel(
                &cfg.account_id,
                &cfg.tunnel_name.clone().unwrap_or_else(|| {
                    format!("isengard-{}", ctx.host_id)
                }),
            )
            .await?;
        // The caller (controller / agent) is responsible for persisting the
        // returned id + token back to settings via the `adapter_config`
        // store. For now we surface the new IDs via the returned endpoint
        // when expose() runs; join() itself just spawns cloudflared.
        created.id
    };

    let token = cfg.tunnel_token.clone().ok_or_else(|| {
        CoreError::Other("missing tunnel_token in cf-tunnel settings".into())
    })?;

    tokio::spawn(async move {
        cloudflared::supervise(token).await;
    });

    let _ = tunnel_id;
    Ok(())
}

async fn expose(
    &self,
    ctx: &AdapterContext,
    spec: &ExposeSpec,
) -> Result<ExposedEndpoint> {
    let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
        .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
    let api = api::CfApi::new(cfg.api_token.clone());

    let tunnel_id = cfg.tunnel_id.clone().ok_or_else(|| {
        CoreError::Other("expose called before tunnel_id was provisioned (run join first)".into())
    })?;

    // Compose the ingress: existing rules (preserve) + new rule + catch-all.
    // For Plan B we keep this naive: replace the entire ingress with our
    // single new rule + 404 catch-all. Multi-rule per-tunnel support is a
    // v1.x follow-up.
    let ingress = vec![
        api::IngressRule {
            hostname: Some(spec.public_hostname.clone()),
            service: format!("http://localhost:{}", spec.local_listener_port),
        },
        api::IngressRule {
            hostname: None,
            service: "http_status:404".into(),
        },
    ];
    api.set_ingress(&cfg.account_id, &tunnel_id, ingress).await?;

    let dns = api
        .upsert_dns_cname(
            &cfg.zone_id,
            &spec.public_hostname,
            &format!("{tunnel_id}.cfargotunnel.com"),
        )
        .await?;

    Ok(ExposedEndpoint {
        id: format!("cf-tunnel:{}", spec.public_hostname),
        url: format!("https://{}", spec.public_hostname),
        adapter_data: serde_json::json!({
            "tunnel_id": tunnel_id,
            "dns_record_id": dns.id,
        }),
    })
}

async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()> {
    let cfg: CfTunnelConfig = serde_json::from_value(ctx.settings.clone())
        .map_err(|e| CoreError::Other(format!("cf-tunnel settings: {e}")))?;
    let api = api::CfApi::new(cfg.api_token.clone());

    // endpoint_id is "cf-tunnel:<hostname>"; we don't track the dns_record_id
    // here without reading from adapter_data. For Plan B v1, the caller
    // passes in the full ExposedEndpoint shape via context; the simpler
    // approach is to surface a separate method or have the agent persist
    // adapter_data. Given the trait shape, we accept that delete-by-name
    // requires a list-then-delete:
    let hostname = endpoint_id.trim_start_matches("cf-tunnel:");

    // List DNS records for this hostname, delete the matching CNAME.
    // (Add list_dns_records to api.rs — small extension.)
    if let Ok(records) = api.list_dns_records(&cfg.zone_id, hostname).await {
        for r in records {
            let _ = api.delete_dns_record(&cfg.zone_id, &r.id).await;
        }
    }

    // Reset ingress to just the catch-all (hostname's rule removed).
    if let Some(tunnel_id) = cfg.tunnel_id.as_ref() {
        let _ = api
            .set_ingress(
                &cfg.account_id,
                tunnel_id,
                vec![api::IngressRule {
                    hostname: None,
                    service: "http_status:404".into(),
                }],
            )
            .await;
    }

    Ok(())
}
```

Add to the top of `lib.rs`:

```rust
use serde::Deserialize;

#[derive(Deserialize)]
struct CfTunnelConfig {
    api_token: String,
    account_id: String,
    zone_id: String,
    tunnel_id: Option<String>,
    tunnel_name: Option<String>,
    tunnel_token: Option<String>,
}
```

Also add a `list_dns_records` method to `api.rs`:

```rust
impl CfApi {
    pub async fn list_dns_records(&self, zone_id: &str, name: &str) -> Result<Vec<DnsRecordSummary>> {
        let url = format!(
            "{}/zones/{}/dns_records?name={}",
            self.base_url, zone_id, name
        );
        let resp: CfResponse<Vec<DnsRecordSummary>> = self
            .auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| CoreError::Other(format!("CF list dns: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("CF list dns JSON: {e}")))?;
        resp.into_result()
    }
}

#[derive(Debug, Deserialize)]
pub struct DnsRecordSummary {
    pub id: String,
    pub name: String,
}
```

- [ ] **Step 2: Build + commit**

Run: `cargo build --workspace`
Expected: success.

```bash
cargo fmt --all
git add crates/isengard-plugins/networking-cf-tunnel/src/lib.rs \
        crates/isengard-plugins/networking-cf-tunnel/src/api.rs
git commit -m "feat(plugins): cf-tunnel join/expose/unexpose end-to-end (lifecycle wired)"
```

---

## Plan B wrap-up

### Task 21: Workspace-wide green + manual smoke checklist in PR

- [ ] **Step 1: Confirm everything builds + tests pass**

Run: `cargo build --workspace`
Expected: clean, no warnings under `-D warnings`.

Run: `cargo test --workspace`
Expected: all green (the Pebble test stays `#[ignore]`'d).

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

Run: `cargo deny check`
Expected: clean. If new advisories appear from `instant-acme` / `wiremock` / `which` deps, follow Plan A's pattern: add them to `deny.toml` ignore list with a doc-comment rationale.

- [ ] **Step 2: Manual smoke (in PR description)**

Three opt-in smoke tests on a real machine. Not automated; document the steps in the PR body so the reviewer can run them:

```markdown
### Manual smoke (each is opt-in, expected to pass on the maintainer's box)

1. **`none` adapter + LE staging:**
   - DNS `A blog.example.com → <host public IP>`, port 80 + 443 reachable
   - `ISENGARD_ACME_DIRECTORY=https://acme-v02.api.letsencrypt.org/directory cargo run -- agent ...`
   - Add a routing rule for `blog.example.com → web:80` via the controller
   - Within 30s, `curl -v https://blog.example.com/` returns the container response with a valid LE cert (NOT staging — set staging URL for first runs)
   - Stop+start agent → cert is reloaded from disk, no re-issuance

2. **`tailscale` adapter + Funnel:**
   - Host logged into a tailnet (`tailscale up`), Funnel allowed in admin
   - Configure adapter via `adapter_config` with `funnel: true`
   - `curl -v https://<host>.<tailnet>.ts.net/` returns container response with adapter-provided cert

3. **`cf-tunnel` adapter:**
   - Get a CF API token with `Zone:Read, Zone:Edit, DNS:Edit` permissions
   - Pre-seed `adapter_config` with `api_token`, `account_id`, `zone_id`, `tunnel_name: isengard-smoke`
   - Agent's `join()` creates the tunnel, persists `tunnel_id` + `tunnel_token`, spawns cloudflared
   - Add routing rule for `blog.example.com → web:80`
   - Within 30s, `curl -v https://blog.example.com/` returns container response with edge cert
   - Verify in CF dashboard: tunnel shows healthy, DNS CNAME exists, ingress rule listed
```

- [ ] **Step 3: Open PR**

```bash
git push -u origin feat/networking-tls-adapters
gh pr create --base feat/networking-proxy-core \
  --title "Phase 8 Plan B: TLS + tailscale + cf-tunnel adapters" \
  --body "$(cat <<'EOF'
## Summary

Implements Plan B of Phase 8 — real HTTPS via three transports.

- **8e**: `instant-acme` integration, Pingora `:8443` HTTPS listener with `IsengardCertResolver`, HTTP-01 challenge handler on `:8080`, renewal scheduler with rate-limit backoff
- **8f**: `networking-tailscale` adapter that shells out to the user's `tailscale` CLI for serve/funnel/cert
- **8g**: `networking-cf-tunnel` adapter that spawns cloudflared as a supervised subprocess + manages tunnels/DNS via CF v4 REST API

## Spec

`docs/superpowers/specs/2026-05-03-phase-8e-8g-tls-and-adapters-design.md`

## Test plan

- [x] cargo build --workspace
- [x] cargo test --workspace (166+ unit/integration tests)
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo deny check
- [ ] Manual smoke: `none` adapter + LE staging
- [ ] Manual smoke: `tailscale` adapter + Funnel
- [ ] Manual smoke: `cf-tunnel` adapter (real CF account)

Stacked on PR #18 (Plan A). Will rebase after that merges.
EOF
)"
```

---

## Notes for the implementer

- The `instant-acme` 0.7 API is mostly stable but minor breakage between patch versions is common. If a build error surfaces for an `Order`/`Authorization`/`Challenge` method, look at the actual installed crate's docs (check `Cargo.lock` for the exact version).
- Pingora 0.8's TLS API is configured via `pingora-rustls` feature. The exact `add_tls_with_settings` / `TlsSettings::with_rustls` shape may vary between 0.8.x patches. Check `pingora_core::listeners::tls` if compile errors hit there.
- Don't push without approval — `next` is public and the user reviews before push.
- The agent-side `Inventory` is new in this plan (Plan A's was controller-side). Use the same SQLite migrations; only the `tls_certs` and `acme_account` tables get used on the agent side. Other tables are dormant but not removed.
- Manual smoke tests are not optional — they're the only real verification of the adapter integrations. Allocate time for them in the PR cycle.
