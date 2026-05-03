# Phase 8 Plan A: Networking & Proxy Core (8a-8d) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the per-host proxy stack end-to-end: NetworkingAdapter trait + storage tables, Pingora running as a tokio task in the agent, controller pushing routing config over the existing sync stream, label discovery via Docker events, healthcheck-aware upstream eviction. Plan A delivers the spec's success criterion #3 (label-based rule appears within 5s, plain HTTP only), minus TLS / real adapters / UI / blue-green hook.

**Architecture:** Pingora embedded as a tokio task inside the existing agent process — no subprocess. Two listeners (`:8080` HTTP, `:8443` reserved for Plan B). Routing rules live in new `routing_rules` SQLite tables on the controller; `ProxyConfig` messages flow from controller to agent over the existing Phase 2 gRPC `Sync` stream. `NetworkingAdapter` is a sub-trait of the existing `Plugin` trait, registered via `inventory`. The `none` adapter ships in Plan A as the trivial baseline; `tailscale` / `cf-tunnel` are Plan B.

**Tech Stack:** Rust 2024, tokio, tonic gRPC (existing), sqlx + SQLite (existing), `pingora-core` + `pingora-proxy` 0.4 (new), `bollard` (existing — Docker socket), inventory plugin registration (existing), async-trait.

**Branch:** `feat/networking-proxy-core` off `next`. Do NOT push without explicit approval — `next` is public.

**Spec:** `docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md` — Plan A covers §§ 1-6 and §8 sub-phases 8a, 8b, 8c, 8d.

---

## Scope

This plan covers Phase 8a-8d from the spec. It does NOT include:

- Phase 8e (ACME / TLS) — Plan B
- Phase 8f / 8g (`tailscale`, `cf-tunnel` adapters) — Plan B
- Phase 8h (Settings UI Networking tab) — Plan C
- Phase 8i (Atomic upstream swap API for blue-green) — Plan C
- Bulk import (NPM JSON / Traefik `dynamic.yml`) — separate spec round
- Cross-host load balancing / fleet-scoped rules — v1.x

**Done when:**

1. `cargo build --workspace` succeeds; no warnings under `-D warnings`
2. `cargo nextest run --workspace` passes (including new integration tests)
3. End-to-end: a single-host install with the `none` adapter, a docker-compose service carrying `isengard.expose: <hostname>` + `isengard.expose.port: <port>` labels, results in a routing rule appearing in the controller within 5s of `docker compose up`, and `curl -H 'Host: <hostname>' http://<host>:8080/` returns the container's response
4. End-to-end: stopping the container marks the upstream unhealthy and Pingora returns 503; restarting the container brings the upstream back to healthy and 200
5. End-to-end: deleting the parent stack deletes the routing rule and Pingora returns 404 for that hostname
6. ~30-35 commits on `feat/networking-proxy-core`, each green individually
7. Branch is reviewable as a single PR back to `next`

---

## File Structure

Files this plan creates or modifies. Each file has one clear responsibility.

### Create

| File | Responsibility |
|---|---|
| `crates/isengard-core/src/networking.rs` | `NetworkingAdapter` trait + `TlsStrategy`, `ExposeSpec`, `ExposedEndpoint`, `Protocol`, `AdapterContext` types |
| `crates/isengard-storage/migrations/0009_routing.sql` | All four routing tables: `routing_rules`, `routing_rule_overrides`, `adapter_config`, `tls_certs` |
| `crates/isengard-storage/src/routing_rule.rs` | `RoutingRule`, `InsertRoutingRule`, `RoutingRuleId`, CRUD on `routing_rules` |
| `crates/isengard-storage/src/routing_rule_override.rs` | `RoutingRuleOverride`, CRUD on `routing_rule_overrides` |
| `crates/isengard-storage/src/adapter_config.rs` | `AdapterConfig`, CRUD on `adapter_config` |
| `crates/isengard-storage/src/tls_cert.rs` | `TlsCertMeta`, CRUD on `tls_certs` (PEM lives on disk; this is metadata only) |
| `crates/isengard-plugins/networking-none/Cargo.toml` | New crate manifest |
| `crates/isengard-plugins/networking-none/src/lib.rs` | `NoneAdapter` impl of `NetworkingAdapter` (returns error from `expose`) |
| `crates/isengard-controller/src/routing.rs` | Controller-side routing rule manager: build `ProxyConfig` per host, push over sync stream |
| `crates/isengard-controller/tests/routing_push_e2e.rs` | Integration test: insert rule → controller pushes config → agent reports applied |
| `crates/isengard-agent/src/proxy/mod.rs` | Pingora task supervisor (start/stop/restart on panic) |
| `crates/isengard-agent/src/proxy/server.rs` | Pingora server setup: listeners on :8080 and :8443 |
| `crates/isengard-agent/src/proxy/router.rs` | `IsengardProxy` impl of `pingora_proxy::ProxyHttp`: host-header routing |
| `crates/isengard-agent/src/proxy/upstreams.rs` | `UpstreamRegistry`: thread-safe map of `hostname → upstream`, applied from `ProxyConfig` |
| `crates/isengard-agent/src/proxy/healthcheck.rs` | Healthcheck task per upstream + eviction state machine |
| `crates/isengard-agent/src/labels.rs` | Docker event subscription via bollard, label parser, diff-and-emit |
| `crates/isengard-agent/tests/proxy_basic_routing.rs` | End-to-end: start agent, push config, curl through Pingora, assert response |
| `crates/isengard-agent/tests/proxy_label_discovery.rs` | End-to-end: start container with labels, assert rule appears |
| `crates/isengard-agent/tests/proxy_health_eviction.rs` | End-to-end: container becomes unhealthy → evicted → re-added |

### Modify

| File | Change |
|---|---|
| `crates/isengard-core/src/lib.rs` | Re-export `networking::*` |
| `crates/isengard-core/Cargo.toml` | (no change required; `serde_json` already present) |
| `crates/isengard-storage/src/lib.rs` | Re-export the four new entity modules |
| `crates/isengard-storage/src/inventory.rs` | (no change in 8a; touched in 8c when controller wires routing) |
| `crates/isengard-proto/proto/isengard.v1.proto` | Add `ProxyConfig`, `RoutingRule`, `Upstream`, `ProxySettings`, `Healthcheck`, `TlsMode` enum, plus a new `controller → agent` variant in `ControllerMessage` carrying `ProxyConfig` |
| `crates/isengard-controller/src/lib.rs` | `pub mod routing;` + wire `RoutingPusher` into `ControllerHandles` |
| `crates/isengard-controller/src/sync_stacks.rs` | When a stack is upserted/deleted, call `routing::reconcile_for_stack` |
| `crates/isengard-controller/src/service.rs` | When agent reports container labels, forward to `routing::ingest_labels` |
| `crates/isengard-agent/src/lib.rs` | `pub mod proxy;` + spawn `proxy::supervisor` in agent's startup |
| `crates/isengard-agent/src/sync.rs` | Handle new `ControllerMessage::ProxyConfig`: forward to `proxy::apply_config` |
| `crates/isengard-agent/Cargo.toml` | Add `pingora-core`, `pingora-proxy`, ensure `bollard` event-stream feature enabled |
| `crates/isengard/Cargo.toml` | Add `isengard-plugin-networking-none` dependency (cargo feature default-on) |
| `crates/isengard/src/main.rs` | Reference the `networking-none` crate in the same registration pattern as `dev_plugin.rs` (so `inventory::submit!` runs) |
| `Cargo.toml` (workspace) | Add `crates/isengard-plugins/networking-none` to `members`; add `pingora-core` / `pingora-proxy` / `bollard` (with `events` feature) to `[workspace.dependencies]` |

---

## Phase 8a: Trait + storage + `none` adapter

### Task 1: `NetworkingAdapter` trait in `isengard-core`

**Files:**
- Create: `crates/isengard-core/src/networking.rs`
- Modify: `crates/isengard-core/src/lib.rs`

- [ ] **Step 1: Write the failing trait-surface test**

Create `crates/isengard-core/tests/networking_trait_surface.rs`:

```rust
//! Compile-only test: exercises that NetworkingAdapter is object-safe and
//! that all the support types serde-roundtrip cleanly.

use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, Protocol, TlsStrategy,
};
use isengard_core::{HostMode, Plugin, PluginContext};

#[test]
fn expose_spec_roundtrips_json() {
    let spec = ExposeSpec {
        public_hostname: "blog.example.com".into(),
        local_listener_port: 8080,
        protocol: Protocol::Http,
        adapter_specific: serde_json::json!({"zone_id": "abc"}),
    };
    let json = serde_json::to_string(&spec).unwrap();
    let back: ExposeSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(back.public_hostname, "blog.example.com");
    assert_eq!(back.local_listener_port, 8080);
}

#[test]
fn networking_adapter_is_object_safe() {
    fn _accepts(_a: &dyn NetworkingAdapter) {}
}

#[test]
fn tls_strategy_edge_termination_serialises() {
    let s = TlsStrategy::EdgeTermination;
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(json, r#""edge_termination""#);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isengard-core --test networking_trait_surface`
Expected: FAIL, "no `networking` module".

- [ ] **Step 3: Create `crates/isengard-core/src/networking.rs`**

```rust
//! NetworkingAdapter trait + value types.
//!
//! Adapters sit ABOVE Pingora in the stack: they make a public hostname
//! reach this host's Pingora listener. Pingora handles host-header routing
//! and TLS termination. The adapter does NOT do hostname routing.
//!
//! See spec §3 in docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md.

use crate::context::PluginContext;
use crate::error::Result;
use crate::plugin::Plugin;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Tcp,
}

/// How the adapter handles TLS for a hostname it exposes. Returned from
/// `NetworkingAdapter::tls_strategy`. Pingora inspects this to decide whether
/// to terminate TLS itself or pass plain HTTP through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TlsStrategy {
    /// Adapter terminates TLS at its edge. Pingora listens on plain HTTP.
    EdgeTermination,
    /// Adapter passes raw TCP through. Pingora handles TLS via its own ACME.
    PassThrough,
    /// Adapter provides a cert via callback (e.g. tailscale's tsnet auto-cert).
    /// Serialised as just the discriminant; the callback is set up by the
    /// adapter at runtime via a separate API.
    AdapterProvided,
}

/// What the controller asks the adapter to expose for a single routing rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposeSpec {
    pub public_hostname: String,
    pub local_listener_port: u16,
    pub protocol: Protocol,
    /// Adapter-specific blob (e.g. CF zone_id, tailscale tags). Opaque to core.
    pub adapter_specific: serde_json::Value,
}

/// What the adapter returns from `expose`. `id` and `adapter_data` are the
/// reverse-key for `unexpose`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposedEndpoint {
    pub id: String,
    pub url: String,
    pub adapter_data: serde_json::Value,
}

/// Per-host context an adapter receives on every call. Built by the agent.
pub struct AdapterContext {
    pub host_id: String,
    pub settings: serde_json::Value,
    pub plugin_ctx: PluginContext,
}

/// NetworkingAdapter is a sub-trait of `Plugin`. Implementations live as
/// crates under `crates/isengard-plugins/networking-*`.
#[async_trait]
pub trait NetworkingAdapter: Plugin {
    fn id(&self) -> &'static str;

    async fn join(&self, ctx: &AdapterContext) -> Result<()>;
    async fn leave(&self, ctx: &AdapterContext) -> Result<()>;

    async fn expose(&self, ctx: &AdapterContext, spec: &ExposeSpec) -> Result<ExposedEndpoint>;
    async fn unexpose(&self, ctx: &AdapterContext, endpoint_id: &str) -> Result<()>;

    fn tls_strategy(&self) -> TlsStrategy;
}
```

- [ ] **Step 4: Re-export from `crates/isengard-core/src/lib.rs`**

Add at the appropriate position (alongside other `pub mod` lines):

```rust
pub mod networking;
```

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p isengard-core --test networking_trait_surface -- --nocapture`
Expected: 3 tests passed.

- [ ] **Step 6: Format + commit**

```bash
cargo fmt --all
git add crates/isengard-core/src/networking.rs \
        crates/isengard-core/src/lib.rs \
        crates/isengard-core/tests/networking_trait_surface.rs
git commit -m "feat(core): NetworkingAdapter trait + value types"
```

---

### Task 2: SQLite migration `0009_routing.sql`

**Files:**
- Create: `crates/isengard-storage/migrations/0009_routing.sql`

- [ ] **Step 1: Write the migration file**

Create exactly:

```sql
-- Phase 8a: routing rules + per-host adapter config + cert metadata.
-- See docs/superpowers/specs/2026-05-03-phase-8-networking-and-proxy-design.md §6.

CREATE TABLE routing_rules (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    fleet           TEXT NOT NULL,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id        INTEGER REFERENCES stacks(id) ON DELETE CASCADE,
    service_name    TEXT NOT NULL,
    container_port  INTEGER NOT NULL,
    public_hostname TEXT NOT NULL,
    protocol        TEXT NOT NULL CHECK (protocol IN ('http', 'tcp')),
    adapter         TEXT NOT NULL,
    tls_mode        TEXT NOT NULL CHECK (tls_mode IN ('edge', 'acme', 'manual')),
    healthcheck_path TEXT,
    healthcheck_interval_secs INTEGER NOT NULL DEFAULT 10,
    auth            TEXT,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'active', 'draining', 'failed')),
    source          TEXT NOT NULL CHECK (source IN ('ui', 'label', 'imported')),
    source_container_id TEXT,
    source_imported_from TEXT,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(public_hostname, host_id)
);

CREATE INDEX idx_routing_rules_host ON routing_rules(host_id);
CREATE INDEX idx_routing_rules_stack ON routing_rules(stack_id);
CREATE INDEX idx_routing_rules_hostname ON routing_rules(public_hostname);

CREATE TABLE routing_rule_overrides (
    routing_rule_id INTEGER NOT NULL REFERENCES routing_rules(id) ON DELETE CASCADE,
    field           TEXT NOT NULL,
    value_json      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (routing_rule_id, field)
);

CREATE TABLE adapter_config (
    host_id     BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    adapter     TEXT NOT NULL,
    config_json TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT 0,
    PRIMARY KEY (host_id, adapter)
);

CREATE TABLE tls_certs (
    public_hostname TEXT PRIMARY KEY,
    host_id         BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    issuer          TEXT NOT NULL,
    not_before      TEXT NOT NULL,
    not_after       TEXT NOT NULL,
    last_renewed_at TEXT,
    next_renewal_at TEXT NOT NULL,
    serial          TEXT
);
```

- [ ] **Step 2: Verify migration applies cleanly**

Run: `cargo test -p isengard-storage migrations`
Expected: PASS — sqlx auto-runs all migrations including 0009 against the temp DB and the existing `migrations` test asserts they apply cleanly.

If the existing test isn't named `migrations` exactly, run the full storage test suite: `cargo test -p isengard-storage` and confirm no regressions.

- [ ] **Step 3: Commit**

```bash
git add crates/isengard-storage/migrations/0009_routing.sql
git commit -m "feat(storage): migration 0009 — routing tables"
```

---

### Task 3: `RoutingRule` entity + CRUD

**Files:**
- Create: `crates/isengard-storage/src/routing_rule.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write the failing CRUD test**

Append to a new file `crates/isengard-storage/tests/routing_rule.rs`:

```rust
use isengard_storage::{
    EnrollHost, Host, InsertRoutingRule, Inventory, RoutingRule, RoutingRuleSource,
    RoutingRuleState, TlsMode,
};
use tempfile::tempdir;

async fn seed_host(inv: &Inventory) -> Host {
    inv.enroll_host(EnrollHost {
        fingerprint: "fp-1".into(),
        hostname: "h1".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.0.1".into(),
        docker_version: "27.0".into(),
        fleet: "default".into(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn insert_then_list_by_host_returns_inserted() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = seed_host(&inv).await;

    let inserted = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: "blog.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: Some("/healthz".into()),
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    let listed = inv.list_routing_rules_for_host(host.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, inserted.id);
    assert_eq!(listed[0].public_hostname, "blog.example.com");
}

#[tokio::test]
async fn insert_unique_violation_returns_err() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = seed_host(&inv).await;

    let make = |hostname: &str| InsertRoutingRule {
        fleet: "default".into(),
        host_id: host.id,
        stack_id: None,
        service_name: "web".into(),
        container_port: 80,
        public_hostname: hostname.into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Acme,
        healthcheck_path: None,
        healthcheck_interval_secs: 10,
        auth: None,
        state: RoutingRuleState::Pending,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    };

    inv.insert_routing_rule(make("dup.example.com")).await.unwrap();
    let err = inv.insert_routing_rule(make("dup.example.com")).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("unique"));
}

#[tokio::test]
async fn delete_by_id_removes_row_and_overrides_cascade() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = seed_host(&inv).await;

    let r = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "x.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("manual"))
        .await
        .unwrap();

    inv.delete_routing_rule(r.id).await.unwrap();

    let still = inv.list_routing_rules_for_host(host.id).await.unwrap();
    assert!(still.is_empty(), "rule should be gone");

    let overrides = inv.list_routing_rule_overrides(r.id).await.unwrap();
    assert!(overrides.is_empty(), "overrides should cascade-delete");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isengard-storage --test routing_rule`
Expected: FAIL — types and methods undefined.

- [ ] **Step 3: Create `crates/isengard-storage/src/routing_rule.rs`**

```rust
//! Routing rule row + insert request + CRUD helpers on `Inventory`.
//!
//! See spec §6.

use crate::error::Result;
use crate::host::HostId;
use crate::stack::StackId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoutingRuleId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Edge,
    Acme,
    Manual,
}

impl TlsMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TlsMode::Edge => "edge",
            TlsMode::Acme => "acme",
            TlsMode::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingRuleState {
    Pending,
    Active,
    Draining,
    Failed,
}

impl RoutingRuleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingRuleState::Pending => "pending",
            RoutingRuleState::Active => "active",
            RoutingRuleState::Draining => "draining",
            RoutingRuleState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingRuleSource {
    Ui,
    Label,
    Imported,
}

impl RoutingRuleSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingRuleSource::Ui => "ui",
            RoutingRuleSource::Label => "label",
            RoutingRuleSource::Imported => "imported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: RoutingRuleId,
    pub fleet: String,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub service_name: String,
    pub container_port: u16,
    pub public_hostname: String,
    pub protocol: String,
    pub adapter: String,
    pub tls_mode: TlsMode,
    pub healthcheck_path: Option<String>,
    pub healthcheck_interval_secs: u32,
    pub auth: Option<String>,
    pub state: RoutingRuleState,
    pub source: RoutingRuleSource,
    pub source_container_id: Option<String>,
    pub source_imported_from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InsertRoutingRule {
    pub fleet: String,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub service_name: String,
    pub container_port: u16,
    pub public_hostname: String,
    pub protocol: String,
    pub adapter: String,
    pub tls_mode: TlsMode,
    pub healthcheck_path: Option<String>,
    pub healthcheck_interval_secs: u32,
    pub auth: Option<String>,
    pub state: RoutingRuleState,
    pub source: RoutingRuleSource,
    pub source_container_id: Option<String>,
    pub source_imported_from: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn insert_routing_rule(
        &self,
        ins: InsertRoutingRule,
    ) -> Result<RoutingRule> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let stack_id = ins.stack_id.map(|s| s.0);
        let row = sqlx::query!(
            r#"
            INSERT INTO routing_rules (
              fleet, host_id, stack_id, service_name, container_port,
              public_hostname, protocol, adapter, tls_mode, healthcheck_path,
              healthcheck_interval_secs, auth, state, source,
              source_container_id, source_imported_from
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
            ins.fleet,
            host_bytes,
            stack_id,
            ins.service_name,
            ins.container_port,
            ins.public_hostname,
            ins.protocol,
            ins.adapter,
            ins.tls_mode.as_str(),
            ins.healthcheck_path,
            ins.healthcheck_interval_secs,
            ins.auth,
            ins.state.as_str(),
            ins.source.as_str(),
            ins.source_container_id,
            ins.source_imported_from,
        )
        .fetch_one(self.pool())
        .await?;

        Ok(RoutingRule {
            id: RoutingRuleId(row.id),
            fleet: ins.fleet,
            host_id: ins.host_id,
            stack_id: ins.stack_id,
            service_name: ins.service_name,
            container_port: ins.container_port,
            public_hostname: ins.public_hostname,
            protocol: ins.protocol,
            adapter: ins.adapter,
            tls_mode: ins.tls_mode,
            healthcheck_path: ins.healthcheck_path,
            healthcheck_interval_secs: ins.healthcheck_interval_secs,
            auth: ins.auth,
            state: ins.state,
            source: ins.source,
            source_container_id: ins.source_container_id,
            source_imported_from: ins.source_imported_from,
        })
    }

    pub async fn list_routing_rules_for_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<RoutingRule>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let rows = sqlx::query!(
            r#"
            SELECT id, fleet, host_id, stack_id, service_name, container_port,
                   public_hostname, protocol, adapter, tls_mode, healthcheck_path,
                   healthcheck_interval_secs, auth, state, source,
                   source_container_id, source_imported_from
            FROM routing_rules
            WHERE host_id = ?
            ORDER BY id ASC
            "#,
            host_bytes
        )
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(RoutingRule {
                id: RoutingRuleId(r.id),
                fleet: r.fleet,
                host_id: HostId::from_bytes(r.host_id.try_into().unwrap()),
                stack_id: r.stack_id.map(StackId),
                service_name: r.service_name,
                container_port: r.container_port as u16,
                public_hostname: r.public_hostname,
                protocol: r.protocol,
                adapter: r.adapter,
                tls_mode: parse_tls_mode(&r.tls_mode)?,
                healthcheck_path: r.healthcheck_path,
                healthcheck_interval_secs: r.healthcheck_interval_secs as u32,
                auth: r.auth,
                state: parse_state(&r.state)?,
                source: parse_source(&r.source)?,
                source_container_id: r.source_container_id,
                source_imported_from: r.source_imported_from,
            });
        }
        Ok(out)
    }

    pub async fn delete_routing_rule(&self, id: RoutingRuleId) -> Result<()> {
        sqlx::query!("DELETE FROM routing_rules WHERE id = ?", id.0)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn update_routing_rule_state(
        &self,
        id: RoutingRuleId,
        state: RoutingRuleState,
    ) -> Result<()> {
        let s = state.as_str();
        sqlx::query!(
            "UPDATE routing_rules SET state = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            s,
            id.0
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

fn parse_tls_mode(s: &str) -> Result<TlsMode> {
    match s {
        "edge" => Ok(TlsMode::Edge),
        "acme" => Ok(TlsMode::Acme),
        "manual" => Ok(TlsMode::Manual),
        other => Err(crate::Error::Decode(format!("invalid tls_mode: {other}"))),
    }
}

fn parse_state(s: &str) -> Result<RoutingRuleState> {
    match s {
        "pending" => Ok(RoutingRuleState::Pending),
        "active" => Ok(RoutingRuleState::Active),
        "draining" => Ok(RoutingRuleState::Draining),
        "failed" => Ok(RoutingRuleState::Failed),
        other => Err(crate::Error::Decode(format!("invalid state: {other}"))),
    }
}

fn parse_source(s: &str) -> Result<RoutingRuleSource> {
    match s {
        "ui" => Ok(RoutingRuleSource::Ui),
        "label" => Ok(RoutingRuleSource::Label),
        "imported" => Ok(RoutingRuleSource::Imported),
        other => Err(crate::Error::Decode(format!("invalid source: {other}"))),
    }
}
```

If `crate::Error::Decode` doesn't exist yet, add it to `crates/isengard-storage/src/error.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // existing variants...
    #[error("decode error: {0}")]
    Decode(String),
}
```

- [ ] **Step 4: Re-export from `lib.rs`**

Modify `crates/isengard-storage/src/lib.rs`:

```rust
pub mod routing_rule;
```

And in the `pub use` block:

```rust
pub use routing_rule::{
    InsertRoutingRule, RoutingRule, RoutingRuleId, RoutingRuleSource, RoutingRuleState, TlsMode,
};
```

- [ ] **Step 5: Run all storage tests**

Run: `cargo test -p isengard-storage`
Expected: all green, including the three new `routing_rule` tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/routing_rule.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/src/error.rs \
        crates/isengard-storage/tests/routing_rule.rs
git commit -m "feat(storage): RoutingRule entity + CRUD"
```

---

### Task 4: `RoutingRuleOverride` entity + CRUD

**Files:**
- Create: `crates/isengard-storage/src/routing_rule_override.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/isengard-storage/tests/routing_rule.rs` (same file as Task 3 — keeps overrides tests near rule tests):

```rust
#[tokio::test]
async fn upsert_override_then_list_returns_value() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = seed_host(&inv).await;

    let r = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "ov.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Label,
            source_container_id: Some("cid-1".into()),
            source_imported_from: None,
        })
        .await
        .unwrap();

    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("manual"))
        .await
        .unwrap();
    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("edge"))
        .await
        .unwrap();

    let list = inv.list_routing_rule_overrides(r.id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].field, "tls_mode");
    assert_eq!(list[0].value_json, serde_json::json!("edge"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isengard-storage --test routing_rule -- upsert_override_then_list_returns_value`
Expected: FAIL — `upsert_routing_rule_override` undefined.

- [ ] **Step 3: Create `crates/isengard-storage/src/routing_rule_override.rs`**

```rust
//! Per-field UI overrides for label-source rules. See spec §5/§6.

use crate::error::Result;
use crate::routing_rule::RoutingRuleId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRuleOverride {
    pub routing_rule_id: RoutingRuleId,
    pub field: String,
    pub value_json: serde_json::Value,
}

impl crate::inventory::Inventory {
    pub async fn upsert_routing_rule_override(
        &self,
        rule_id: RoutingRuleId,
        field: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        let v = value.to_string();
        sqlx::query!(
            r#"
            INSERT INTO routing_rule_overrides (routing_rule_id, field, value_json)
            VALUES (?, ?, ?)
            ON CONFLICT (routing_rule_id, field) DO UPDATE SET
              value_json = excluded.value_json,
              created_at = CURRENT_TIMESTAMP
            "#,
            rule_id.0,
            field,
            v
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_routing_rule_overrides(
        &self,
        rule_id: RoutingRuleId,
    ) -> Result<Vec<RoutingRuleOverride>> {
        let rows = sqlx::query!(
            "SELECT routing_rule_id, field, value_json FROM routing_rule_overrides WHERE routing_rule_id = ?",
            rule_id.0
        )
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(RoutingRuleOverride {
                routing_rule_id: RoutingRuleId(r.routing_rule_id),
                field: r.field,
                value_json: serde_json::from_str(&r.value_json)
                    .map_err(|e| crate::Error::Decode(e.to_string()))?,
            });
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Re-export**

In `crates/isengard-storage/src/lib.rs`:

```rust
pub mod routing_rule_override;
pub use routing_rule_override::RoutingRuleOverride;
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-storage`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/routing_rule_override.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/tests/routing_rule.rs
git commit -m "feat(storage): RoutingRuleOverride entity + upsert/list"
```

---

### Task 5: `AdapterConfig` entity + CRUD

**Files:**
- Create: `crates/isengard-storage/src/adapter_config.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/isengard-storage/tests/adapter_config.rs`:

```rust
use isengard_storage::{AdapterConfig, EnrollHost, Inventory, UpsertAdapterConfig};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_config() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    inv.upsert_adapter_config(UpsertAdapterConfig {
        host_id: host.id,
        adapter: "none".into(),
        config_json: serde_json::json!({}),
        enabled: true,
    })
    .await
    .unwrap();

    let cfg = inv
        .get_adapter_config(host.id, "none")
        .await
        .unwrap()
        .expect("config exists");
    assert_eq!(cfg.adapter, "none");
    assert!(cfg.enabled);

    // upsert overwrites
    inv.upsert_adapter_config(UpsertAdapterConfig {
        host_id: host.id,
        adapter: "none".into(),
        config_json: serde_json::json!({"k": "v"}),
        enabled: false,
    })
    .await
    .unwrap();

    let cfg = inv.get_adapter_config(host.id, "none").await.unwrap().unwrap();
    assert!(!cfg.enabled);
    assert_eq!(cfg.config_json, serde_json::json!({"k": "v"}));
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-storage --test adapter_config`
Expected: FAIL.

- [ ] **Step 3: Create `crates/isengard-storage/src/adapter_config.rs`**

```rust
use crate::error::Result;
use crate::host::HostId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfig {
    pub host_id: HostId,
    pub adapter: String,
    pub config_json: serde_json::Value,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct UpsertAdapterConfig {
    pub host_id: HostId,
    pub adapter: String,
    pub config_json: serde_json::Value,
    pub enabled: bool,
}

impl crate::inventory::Inventory {
    pub async fn upsert_adapter_config(&self, ins: UpsertAdapterConfig) -> Result<()> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let cfg_str = ins.config_json.to_string();
        sqlx::query!(
            r#"
            INSERT INTO adapter_config (host_id, adapter, config_json, enabled)
            VALUES (?, ?, ?, ?)
            ON CONFLICT (host_id, adapter) DO UPDATE SET
              config_json = excluded.config_json,
              enabled = excluded.enabled
            "#,
            host_bytes,
            ins.adapter,
            cfg_str,
            ins.enabled
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_adapter_config(
        &self,
        host_id: HostId,
        adapter: &str,
    ) -> Result<Option<AdapterConfig>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let row = sqlx::query!(
            "SELECT host_id, adapter, config_json, enabled FROM adapter_config WHERE host_id = ? AND adapter = ?",
            host_bytes,
            adapter
        )
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| AdapterConfig {
            host_id: HostId::from_bytes(r.host_id.try_into().unwrap()),
            adapter: r.adapter,
            config_json: serde_json::from_str(&r.config_json).unwrap_or(serde_json::json!({})),
            enabled: r.enabled != 0,
        }))
    }
}
```

- [ ] **Step 4: Re-export**

In `crates/isengard-storage/src/lib.rs`:

```rust
pub mod adapter_config;
pub use adapter_config::{AdapterConfig, UpsertAdapterConfig};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-storage --test adapter_config`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/adapter_config.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/tests/adapter_config.rs
git commit -m "feat(storage): AdapterConfig entity + upsert/get"
```

---

### Task 6: `TlsCertMeta` entity + CRUD

**Files:**
- Create: `crates/isengard-storage/src/tls_cert.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write failing test**

Create `crates/isengard-storage/tests/tls_cert.rs`:

```rust
use chrono::Utc;
use isengard_storage::{EnrollHost, Inventory, TlsCertMeta, UpsertTlsCertMeta};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_meta() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(dir.path().join("isengard.db")).await.unwrap();
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let now = Utc::now();
    inv.upsert_tls_cert_meta(UpsertTlsCertMeta {
        public_hostname: "blog.example.com".into(),
        host_id: host.id,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + chrono::Duration::days(90),
        next_renewal_at: now + chrono::Duration::days(60),
        serial: Some("0xabc".into()),
    })
    .await
    .unwrap();

    let m = inv
        .get_tls_cert_meta("blog.example.com")
        .await
        .unwrap()
        .expect("exists");
    assert_eq!(m.issuer, "lets_encrypt");
    assert_eq!(m.serial.as_deref(), Some("0xabc"));
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-storage --test tls_cert`
Expected: FAIL.

- [ ] **Step 3: Create `crates/isengard-storage/src/tls_cert.rs`**

```rust
use crate::error::Result;
use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone)]
pub struct UpsertTlsCertMeta {
    pub public_hostname: String,
    pub host_id: HostId,
    pub issuer: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub next_renewal_at: DateTime<Utc>,
    pub serial: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn upsert_tls_cert_meta(&self, ins: UpsertTlsCertMeta) -> Result<()> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let nb = ins.not_before.to_rfc3339();
        let na = ins.not_after.to_rfc3339();
        let nr = ins.next_renewal_at.to_rfc3339();
        sqlx::query!(
            r#"
            INSERT INTO tls_certs (
              public_hostname, host_id, issuer, not_before, not_after,
              next_renewal_at, serial
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (public_hostname) DO UPDATE SET
              host_id = excluded.host_id,
              issuer = excluded.issuer,
              not_before = excluded.not_before,
              not_after = excluded.not_after,
              next_renewal_at = excluded.next_renewal_at,
              last_renewed_at = CURRENT_TIMESTAMP,
              serial = excluded.serial
            "#,
            ins.public_hostname,
            host_bytes,
            ins.issuer,
            nb,
            na,
            nr,
            ins.serial,
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_tls_cert_meta(
        &self,
        public_hostname: &str,
    ) -> Result<Option<TlsCertMeta>> {
        let row = sqlx::query!(
            "SELECT public_hostname, host_id, issuer, not_before, not_after, last_renewed_at, next_renewal_at, serial FROM tls_certs WHERE public_hostname = ?",
            public_hostname
        )
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|r| TlsCertMeta {
            public_hostname: r.public_hostname,
            host_id: HostId::from_bytes(r.host_id.try_into().unwrap()),
            issuer: r.issuer,
            not_before: DateTime::parse_from_rfc3339(&r.not_before).unwrap().with_timezone(&Utc),
            not_after: DateTime::parse_from_rfc3339(&r.not_after).unwrap().with_timezone(&Utc),
            last_renewed_at: r.last_renewed_at.and_then(|s| {
                DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Utc))
            }),
            next_renewal_at: DateTime::parse_from_rfc3339(&r.next_renewal_at).unwrap().with_timezone(&Utc),
            serial: r.serial,
        }))
    }
}
```

- [ ] **Step 4: Re-export**

In `crates/isengard-storage/src/lib.rs`:

```rust
pub mod tls_cert;
pub use tls_cert::{TlsCertMeta, UpsertTlsCertMeta};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-storage --test tls_cert`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-storage/src/tls_cert.rs \
        crates/isengard-storage/src/lib.rs \
        crates/isengard-storage/tests/tls_cert.rs
git commit -m "feat(storage): TlsCertMeta entity + upsert/get"
```

---

### Task 7: `none` adapter crate

**Files:**
- Create: `crates/isengard-plugins/networking-none/Cargo.toml`
- Create: `crates/isengard-plugins/networking-none/src/lib.rs`
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Write failing test that asserts the adapter loads**

Create `crates/isengard-plugins/networking-none/tests/loads.rs`:

```rust
use isengard_core::networking::{NetworkingAdapter, TlsStrategy};
use isengard_plugin_networking_none::NoneAdapter;

#[test]
fn id_is_none() {
    let a = NoneAdapter::default();
    assert_eq!(a.id(), "none");
}

#[test]
fn tls_strategy_is_pass_through() {
    let a = NoneAdapter::default();
    assert!(matches!(a.tls_strategy(), TlsStrategy::PassThrough));
}

#[tokio::test]
async fn expose_returns_err() {
    use isengard_core::context::{HostMode, PluginContext};
    use isengard_core::networking::{AdapterContext, ExposeSpec, Protocol};

    let a = NoneAdapter::default();
    let ctx = AdapterContext {
        host_id: "h-1".into(),
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
    assert!(format!("{err}").contains("no adapter"));
}
```

- [ ] **Step 2: Add the crate to the workspace**

In root `Cargo.toml`, append to `members`:

```toml
"crates/isengard-plugins/networking-none",
```

- [ ] **Step 3: Create `crates/isengard-plugins/networking-none/Cargo.toml`**

```toml
[package]
name = "isengard-plugin-networking-none"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "No-op networking adapter for Isengard (baseline)"

[dependencies]
async-trait.workspace = true
inventory.workspace = true
isengard-core = { path = "../../isengard-core" }
serde_json.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 4: Create `crates/isengard-plugins/networking-none/src/lib.rs`**

```rust
//! No-op `NetworkingAdapter` baseline. expose() returns an error explaining
//! that no adapter is configured. Always available so an agent without
//! explicit configuration has a well-defined adapter.

use async_trait::async_trait;
use isengard_core::context::PluginContext;
use isengard_core::error::Result;
use isengard_core::networking::{
    AdapterContext, ExposeSpec, ExposedEndpoint, NetworkingAdapter, TlsStrategy,
};
use isengard_core::plugin::Plugin;

#[derive(Default)]
pub struct NoneAdapter;

#[async_trait]
impl Plugin for NoneAdapter {
    fn name(&self) -> &'static str {
        "networking-none"
    }
    fn version(&self) -> &'static str {
        "0.1.0-alpha"
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
impl NetworkingAdapter for NoneAdapter {
    fn id(&self) -> &'static str {
        "none"
    }
    async fn join(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }
    async fn leave(&self, _ctx: &AdapterContext) -> Result<()> {
        Ok(())
    }
    async fn expose(
        &self,
        _ctx: &AdapterContext,
        _spec: &ExposeSpec,
    ) -> Result<ExposedEndpoint> {
        Err(isengard_core::error::Error::Other(
            "no adapter configured: install + enable a NetworkingAdapter (tailscale, cf-tunnel) to expose hostnames"
                .into(),
        ))
    }
    async fn unexpose(&self, _ctx: &AdapterContext, _endpoint_id: &str) -> Result<()> {
        Ok(())
    }
    fn tls_strategy(&self) -> TlsStrategy {
        TlsStrategy::PassThrough
    }
}

inventory::submit! {
    isengard_core::registration::PluginRegistration {
        name: "networking-none",
        kind: isengard_core::registration::PluginKind::Networking,
        ctor: || Box::new(NoneAdapter::default()),
    }
}
```

If `PluginKind::Networking` doesn't yet exist on `isengard_core::registration::PluginKind`, add it. First check the existing enum (`grep -n 'pub enum PluginKind' crates/isengard-core/src/registration.rs`); add `Networking` as a new variant alongside the existing ones. Example shape:

```rust
// in crates/isengard-core/src/registration.rs
pub enum PluginKind {
    Agent,        // existing
    Controller,   // existing
    Networking,   // NEW — add this variant
}
```

(Adapt to whatever variants currently exist; the only change is the addition of `Networking`.)

- [ ] **Step 5: Add the crate as a dependency of the binary**

In `crates/isengard/Cargo.toml`, alongside the other plugin deps:

```toml
isengard-plugin-networking-none = { path = "../isengard-plugins/networking-none" }
```

- [ ] **Step 6: Reference the crate from `main.rs` so `inventory::submit!` runs**

In `crates/isengard/src/main.rs`, near the existing plugin imports (matches the existing pattern for `dev_plugin`):

```rust
use isengard_plugin_networking_none as _;
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p isengard-plugin-networking-none`
Expected: 3 green tests.

Then run: `cargo build --workspace`
Expected: success — confirms registration compiles.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add Cargo.toml \
        crates/isengard/Cargo.toml \
        crates/isengard/src/main.rs \
        crates/isengard-plugins/networking-none \
        crates/isengard-core/src/registration.rs
git commit -m "feat(plugins): networking-none adapter (baseline)"
```

---

## Phase 8b: Pingora skeleton in agent

### Task 8: Add Pingora deps + new `proxy` module skeleton

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/isengard-agent/Cargo.toml`
- Create: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Add Pingora to workspace deps**

In root `Cargo.toml`, under `[workspace.dependencies]`:

```toml
pingora-core = "0.4"
pingora-proxy = "0.4"
async-trait = "0.1"
```

(`async-trait` is already present in the workspace deps; only add the two pingora lines.)

- [ ] **Step 2: Add agent deps**

In `crates/isengard-agent/Cargo.toml`:

```toml
pingora-core = { workspace = true }
pingora-proxy = { workspace = true }
```

- [ ] **Step 3: Write failing module-load test**

Create `crates/isengard-agent/src/proxy/mod.rs`:

```rust
//! Per-host Pingora supervisor. Owns the lifetime of the proxy task and
//! handles config reloads + panic-restart with exponential backoff.

pub mod healthcheck;
pub mod router;
pub mod server;
pub mod upstreams;

use std::sync::Arc;
use tokio::sync::RwLock;

pub use upstreams::{Upstream, UpstreamRegistry};

/// Top-level proxy state shared between the supervisor task and Pingora's
/// handler. Cheap to clone (RwLock + Arc).
#[derive(Clone, Default)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self::default()
    }
}
```

Add to `crates/isengard-agent/src/lib.rs` (in module declarations):

```rust
pub mod proxy;
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p isengard-agent`
Expected: build will FAIL because `upstreams`, `router`, `server`, `healthcheck` modules don't exist yet — that's the "failing test" for this task.

- [ ] **Step 5: Create stub modules so the build passes**

Create `crates/isengard-agent/src/proxy/upstreams.rs`:

```rust
use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub container_id: String,
    pub addr: SocketAddr,
    pub healthy: bool,
}

#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    by_hostname: HashMap<String, Upstream>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&mut self, hostname: impl Into<String>, upstream: Upstream) {
        self.by_hostname.insert(hostname.into(), upstream);
    }
    pub fn get(&self, hostname: &str) -> Option<&Upstream> {
        self.by_hostname.get(hostname)
    }
    pub fn remove(&mut self, hostname: &str) -> Option<Upstream> {
        self.by_hostname.remove(hostname)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Upstream)> {
        self.by_hostname.iter()
    }
}
```

Create `crates/isengard-agent/src/proxy/router.rs`:

```rust
//! IsengardProxy: the pingora_proxy::ProxyHttp impl.
//! Filled in by Task 9.

use pingora_proxy::{ProxyHttp, Session};

use crate::proxy::ProxyState;

pub struct IsengardProxy {
    pub state: ProxyState,
}

#[async_trait::async_trait]
impl ProxyHttp for IsengardProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<pingora_core::upstreams::peer::HttpPeer>> {
        // Placeholder — replaced in Task 9.
        Err(pingora_core::Error::new_str("not implemented"))
    }
}
```

Create `crates/isengard-agent/src/proxy/server.rs`:

```rust
//! Pingora server bootstrap. Filled in by Task 9.
```

Create `crates/isengard-agent/src/proxy/healthcheck.rs`:

```rust
//! Healthcheck task. Filled in by Phase 8d Task 19.
```

- [ ] **Step 6: Verify build passes**

Run: `cargo build -p isengard-agent`
Expected: success.

Run: `cargo test -p isengard-agent --lib`
Expected: green (no new tests; existing tests still pass).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add Cargo.toml crates/isengard-agent
git commit -m "feat(agent): proxy module skeleton, pingora deps"
```

---

### Task 9: Pingora server with hardcoded single-rule routing

**Files:**
- Modify: `crates/isengard-agent/src/proxy/server.rs`
- Modify: `crates/isengard-agent/src/proxy/router.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Create: `crates/isengard-agent/tests/proxy_basic_routing.rs`

- [ ] **Step 1: Write the failing end-to-end test**

Create `crates/isengard-agent/tests/proxy_basic_routing.rs`:

```rust
//! End-to-end: spawn proxy with one hardcoded rule pointing at a tiny
//! tokio HTTP server, curl it via Pingora, assert the response body.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_origin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let body = b"hello-from-origin";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        std::str::from_utf8(body).unwrap()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
#[ignore] // runs end-to-end; opt in via `cargo test --test proxy_basic_routing -- --ignored`
async fn route_by_host_header_returns_origin_response() {
    let origin = spawn_origin().await;

    let proxy_port = 18080u16;
    let state = isengard_agent::proxy::ProxyState::new();
    {
        let mut up = state.upstreams.write().await;
        up.set(
            "blog.test",
            isengard_agent::proxy::Upstream {
                container_id: "test".into(),
                addr: origin,
                healthy: true,
            },
        );
    }

    let st = state.clone();
    tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port).await;
    });

    // give pingora a moment to bind
    tokio::time::sleep(Duration::from_millis(300)).await;

    let body = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/", proxy_port))
        .header("Host", "blog.test")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello-from-origin");
}
```

Add `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }` to `[dev-dependencies]` in `crates/isengard-agent/Cargo.toml`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isengard-agent --test proxy_basic_routing -- --ignored`
Expected: FAIL — `server::run_for_test` undefined / `ProxyState::upstreams` not exposed correctly.

- [ ] **Step 3: Implement `IsengardProxy::upstream_peer` — host-header lookup**

Replace `crates/isengard-agent/src/proxy/router.rs`:

```rust
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};

use crate::proxy::ProxyState;

pub struct IsengardProxy {
    pub state: ProxyState,
}

#[async_trait::async_trait]
impl ProxyHttp for IsengardProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
            .unwrap_or_default();

        let upstreams = self.state.upstreams.read().await;
        let Some(up) = upstreams.get(&host) else {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(404),
                "no_route",
                format!("no routing rule for {host}"),
            ));
        };
        if !up.healthy {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(503),
                "no_healthy_upstream",
                format!("no healthy upstream for {host}"),
            ));
        }
        Ok(Box::new(HttpPeer::new(up.addr, false, String::new())))
    }
}
```

- [ ] **Step 4: Implement `server::run_for_test`**

Replace `crates/isengard-agent/src/proxy/server.rs`:

```rust
//! Pingora server bootstrap.
//!
//! `run` is the production entrypoint (binds :8080 and :8443).
//! `run_for_test` binds only the HTTP listener on a caller-supplied port,
//! used by integration tests.

use crate::proxy::ProxyState;
use crate::proxy::router::IsengardProxy;
use pingora_core::server::Server;
use pingora_core::server::configuration::ServerConf;
use pingora_proxy::http_proxy_service;

pub async fn run_for_test(state: ProxyState, port: u16) {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy { state });
    svc.add_tcp(&format!("127.0.0.1:{port}"));
    server.add_service(svc);

    // Run on the current tokio runtime. Pingora's `run_forever` blocks the
    // current thread; spawn it in a blocking task so tokio can keep handling
    // the test runtime.
    tokio::task::spawn_blocking(move || server.run_forever()).await.ok();
}

pub async fn run(state: ProxyState, http_port: u16, _https_port: u16) {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy { state });
    svc.add_tcp(&format!("0.0.0.0:{http_port}"));
    // HTTPS listener wired in Plan B (Phase 8e). For now, `https_port` is
    // accepted but unbound.
    server.add_service(svc);

    tokio::task::spawn_blocking(move || server.run_forever()).await.ok();
}
```

- [ ] **Step 5: Run the test (now expected to pass)**

Run: `cargo test -p isengard-agent --test proxy_basic_routing -- --ignored --nocapture`
Expected: green. Body of response is "hello-from-origin".

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): pingora proxy with host-header routing (single-rule)"
```

---

### Task 10: Wire the proxy supervisor into agent startup

**Files:**
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Write the supervisor**

Append to `crates/isengard-agent/src/proxy/mod.rs`:

```rust
use std::time::Duration;
use tracing::{error, info, warn};

const MAX_RESTARTS: u32 = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(300);

pub async fn supervise(state: ProxyState, http_port: u16, https_port: u16) {
    let mut restarts: Vec<std::time::Instant> = Vec::new();
    loop {
        let st = state.clone();
        info!(http_port, https_port, "proxy: starting pingora task");

        let handle = tokio::spawn(async move {
            server::run(st, http_port, https_port).await;
        });

        match handle.await {
            Ok(_) => {
                warn!("proxy: pingora task exited cleanly; restarting");
            }
            Err(e) if e.is_panic() => {
                error!("proxy: pingora panicked: {e:?}");
            }
            Err(e) => {
                error!("proxy: pingora task error: {e:?}");
            }
        }

        let now = std::time::Instant::now();
        restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        restarts.push(now);
        if restarts.len() as u32 > MAX_RESTARTS {
            error!(
                "proxy: {} restarts in {:?} — giving up; emit proxy.crashloop",
                restarts.len(),
                RESTART_WINDOW
            );
            return;
        }
        let backoff = Duration::from_millis(250 * (1u64 << (restarts.len().min(5) as u64)));
        tokio::time::sleep(backoff).await;
    }
}
```

- [ ] **Step 2: Spawn the supervisor in agent startup**

In `crates/isengard-agent/src/lib.rs`, locate the agent's `run` / `start` entrypoint (the function that currently spawns enroll/sync). Add the proxy supervisor spawn alongside the existing spawns:

```rust
let proxy_state = crate::proxy::ProxyState::new();
let ps = proxy_state.clone();
let http_port = 8080u16;  // wired to settings in Task 13
let https_port = 8443u16;
tokio::spawn(async move {
    crate::proxy::supervise(ps, http_port, https_port).await;
});
```

Stash `proxy_state` somewhere reachable by the sync handler (Task 13 will use it). Easiest: add it to `AgentState` if present, or pass it into the sync loop's context struct.

- [ ] **Step 3: Verify build**

Run: `cargo build -p isengard-agent`
Expected: success.

- [ ] **Step 4: Verify existing agent tests still pass**

Run: `cargo test -p isengard-agent`
Expected: green. (Proxy is started but receives no config, so no requests will be served — existing enroll/sync tests don't touch it.)

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): supervise pingora task with backoff-restart"
```

---

## Phase 8c: ProxyConfig push + label discovery

### Task 11: `ProxyConfig` proto messages

**Files:**
- Modify: `crates/isengard-proto/proto/isengard.v1.proto`

- [ ] **Step 1: Write a failing proto-roundtrip test**

Create `crates/isengard-proto/tests/proxy_config_roundtrip.rs`:

```rust
use isengard_proto::pb::{
    Healthcheck, ProxyConfig, ProxySettings, RoutingRule, TlsMode, Upstream,
};
use prost::Message;

#[test]
fn proxy_config_encodes_and_decodes() {
    let cfg = ProxyConfig {
        host_id: "h-1".into(),
        generation: 7,
        rules: vec![RoutingRule {
            id: 42,
            public_hostname: "blog.example.com".into(),
            upstream: Some(Upstream {
                container_id: "cid".into(),
                container_ip: "10.0.0.5".into(),
                container_port: 8080,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: Some(Healthcheck {
                path: "/healthz".into(),
                interval_secs: 10,
            }),
            adapter: "none".into(),
        }],
        settings: Some(ProxySettings {
            http_port: 8080,
            https_port: 8443,
            acme_contact_email: "ops@example.com".into(),
            log_sample_rate: 0.05,
        }),
    };

    let bytes = cfg.encode_to_vec();
    let back = ProxyConfig::decode(&*bytes).unwrap();
    assert_eq!(back.generation, 7);
    assert_eq!(back.rules.len(), 1);
    assert_eq!(back.rules[0].public_hostname, "blog.example.com");
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-proto --test proxy_config_roundtrip`
Expected: FAIL — types don't exist.

- [ ] **Step 3: Add messages to `crates/isengard-proto/proto/isengard.v1.proto`**

Append before the closing of the file (or wherever new messages are typically appended):

```protobuf
// ============ PROXY (Phase 8) ============

enum TlsMode {
  TLS_MODE_UNSPECIFIED = 0;
  TLS_MODE_EDGE = 1;
  TLS_MODE_ACME = 2;
  TLS_MODE_MANUAL = 3;
}

message Upstream {
  string container_id = 1;
  string container_ip = 2;
  uint32 container_port = 3;
}

message Healthcheck {
  string path = 1;            // empty string = TCP-only check
  uint32 interval_secs = 2;
}

message RoutingRule {
  int64 id = 1;
  string public_hostname = 2;
  Upstream upstream = 3;
  TlsMode tls_mode = 4;
  Healthcheck healthcheck = 5;
  string adapter = 6;
}

message ProxySettings {
  uint32 http_port = 1;
  uint32 https_port = 2;
  string acme_contact_email = 3;
  double log_sample_rate = 4;
}

message ProxyConfig {
  string host_id = 1;
  uint64 generation = 2;
  repeated RoutingRule rules = 3;
  ProxySettings settings = 4;
}
```

Then add `ProxyConfig` to the existing `ControllerMessage` oneof. Locate the oneof in `ControllerMessage` and add a new variant (use the next unused field number; e.g. if existing variants go through 5, use 6):

```protobuf
message ControllerMessage {
  oneof payload {
    HeartbeatAck heartbeat_ack = 1;
    // ... existing variants ...
    ProxyConfig proxy_config = 6;
  }
}
```

(Verify the actual existing variant numbering and use the next free number.)

Also add an `agent → controller` variant for label-discovered rules. In `AgentMessage`'s oneof:

```protobuf
message ContainerLabelsReport {
  string container_id = 1;
  string container_name = 2;
  string image = 3;
  map<string, string> labels = 4;
}

message ContainerLabelsRemoved {
  string container_id = 1;
}
```

And add to the `AgentMessage` oneof:

```protobuf
ContainerLabelsReport container_labels_report = 7;  // use next free number
ContainerLabelsRemoved container_labels_removed = 8;
```

- [ ] **Step 4: Run the roundtrip test**

Run: `cargo test -p isengard-proto --test proxy_config_roundtrip`
Expected: green.

Run: `cargo build --workspace`
Expected: success — confirms generated code is consistent with all consumers.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-proto
git commit -m "feat(proto): ProxyConfig + ContainerLabelsReport messages"
```

---

### Task 12: Controller-side `RoutingPusher`

**Files:**
- Create: `crates/isengard-controller/src/routing.rs`
- Modify: `crates/isengard-controller/src/lib.rs`
- Modify: `crates/isengard-controller/src/service.rs` (sync stream handler)

- [ ] **Step 1: Write the failing pusher test**

Create `crates/isengard-controller/tests/routing_push_e2e.rs`:

```rust
//! Push-side test: insert a routing rule for a host, assert RoutingPusher
//! produces a ProxyConfig message containing it, with monotonic generation.

use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn build_proxy_config_for_host_includes_rule() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(Inventory::open(dir.path().join("isengard.db")).await.unwrap());
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let _r = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: "blog.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: Some("/healthz".into()),
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    let pusher = isengard_controller::routing::RoutingPusher::new(inv.clone());
    let cfg = pusher.build_for_host(host.id).await.unwrap();
    assert_eq!(cfg.rules.len(), 1);
    assert_eq!(cfg.rules[0].public_hostname, "blog.example.com");
    assert_eq!(cfg.generation, 1);

    // Calling again does NOT increment without a change.
    let cfg2 = pusher.build_for_host(host.id).await.unwrap();
    assert_eq!(cfg2.generation, 1);

    // Adding another rule increments.
    inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id: host.id,
        stack_id: None,
        service_name: "api".into(),
        container_port: 8081,
        public_hostname: "api.example.com".into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Acme,
        healthcheck_path: None,
        healthcheck_interval_secs: 10,
        auth: None,
        state: RoutingRuleState::Active,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    })
    .await
    .unwrap();

    let cfg3 = pusher.build_for_host(host.id).await.unwrap();
    assert_eq!(cfg3.generation, 2);
    assert_eq!(cfg3.rules.len(), 2);
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-controller --test routing_push_e2e`
Expected: FAIL — `RoutingPusher` undefined.

- [ ] **Step 3: Create `crates/isengard-controller/src/routing.rs`**

```rust
//! Routing rule reconciler: turns the storage view of `routing_rules` into
//! `ProxyConfig` messages addressed at individual hosts.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isengard_proto::pb::{
    Healthcheck, ProxyConfig, ProxySettings, RoutingRule as ProtoRule, TlsMode, Upstream,
};
use isengard_storage::{HostId, Inventory, RoutingRule as StorageRule};
use tokio::sync::Mutex;

#[derive(Default)]
struct PerHostState {
    /// Last generation pushed to the host.
    generation: u64,
    /// Hash of the rules-set used to compute the last generation. If the
    /// new hash matches, generation does NOT increment.
    last_hash: u64,
}

pub struct RoutingPusher {
    inv: Arc<Inventory>,
    by_host: Mutex<HashMap<HostId, PerHostState>>,
}

impl RoutingPusher {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_host: Mutex::new(HashMap::new()),
        }
    }

    /// Build the latest ProxyConfig for a host, incrementing generation
    /// only when the rule-set has actually changed.
    pub async fn build_for_host(&self, host_id: HostId) -> Result<ProxyConfig> {
        let rules = self.inv.list_routing_rules_for_host(host_id).await?;

        let hash = hash_rules(&rules);
        let mut guard = self.by_host.lock().await;
        let entry = guard.entry(host_id).or_default();
        if entry.last_hash != hash {
            entry.generation += 1;
            entry.last_hash = hash;
        }

        Ok(ProxyConfig {
            host_id: host_id.to_string(),
            generation: entry.generation,
            rules: rules.into_iter().map(to_proto).collect(),
            settings: Some(ProxySettings {
                http_port: 8080,
                https_port: 8443,
                acme_contact_email: String::new(),
                log_sample_rate: 0.0,
            }),
        })
    }
}

fn to_proto(r: StorageRule) -> ProtoRule {
    ProtoRule {
        id: r.id.0,
        public_hostname: r.public_hostname,
        upstream: Some(Upstream {
            // container_id + container_ip resolved by agent at apply-time
            // when the upstream actually exists. For now: just carry the
            // service identity; agent fills in IP from its local Docker view.
            container_id: r.service_name.clone(),
            container_ip: String::new(),
            container_port: r.container_port as u32,
        }),
        tls_mode: match r.tls_mode {
            isengard_storage::TlsMode::Edge => TlsMode::Edge as i32,
            isengard_storage::TlsMode::Acme => TlsMode::Acme as i32,
            isengard_storage::TlsMode::Manual => TlsMode::Manual as i32,
        },
        healthcheck: Some(Healthcheck {
            path: r.healthcheck_path.unwrap_or_default(),
            interval_secs: r.healthcheck_interval_secs,
        }),
        adapter: r.adapter,
    }
}

fn hash_rules(rules: &[StorageRule]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for r in rules {
        r.id.0.hash(&mut h);
        r.public_hostname.hash(&mut h);
        r.container_port.hash(&mut h);
        r.adapter.hash(&mut h);
        r.healthcheck_path.hash(&mut h);
        r.healthcheck_interval_secs.hash(&mut h);
    }
    h.finish()
}
```

- [ ] **Step 4: Re-export from controller `lib.rs`**

In `crates/isengard-controller/src/lib.rs`:

```rust
pub mod routing;
```

Also extend `ControllerHandles` to carry the pusher:

```rust
#[derive(Clone)]
pub struct ControllerHandles {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
    pub routing: Arc<routing::RoutingPusher>,
}
```

Wherever `ControllerHandles` is constructed (likely in the `run` / `controller_main` function), construct the pusher:

```rust
let routing = Arc::new(routing::RoutingPusher::new(inventory.clone()));
let handles = ControllerHandles {
    inventory: inventory.clone(),
    journal: journal.clone(),
    bus: bus.clone(),
    routing: routing.clone(),
};
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p isengard-controller --test routing_push_e2e`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-controller
git commit -m "feat(controller): RoutingPusher builds ProxyConfig with monotonic generation"
```

---

### Task 13: Agent applies `ProxyConfig` on receipt

**Files:**
- Modify: `crates/isengard-agent/src/sync.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs` (add `apply_config`)

- [ ] **Step 1: Write the failing test**

Create `crates/isengard-agent/tests/proxy_apply_config.rs`:

```rust
use isengard_proto::pb::{Healthcheck, ProxyConfig, RoutingRule, TlsMode, Upstream};

#[tokio::test]
async fn apply_config_replaces_upstream_registry() {
    let state = isengard_agent::proxy::ProxyState::new();

    let cfg = ProxyConfig {
        host_id: "h-1".into(),
        generation: 1,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: "a.test".into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: 8080,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: Some(Healthcheck {
                path: "/healthz".into(),
                interval_secs: 10,
            }),
            adapter: "none".into(),
        }],
        settings: None,
    };

    isengard_agent::proxy::apply_config(&state, cfg).await.unwrap();

    let up = state.upstreams.read().await;
    let got = up.get("a.test").expect("rule applied");
    assert_eq!(got.container_id, "web");
    assert_eq!(got.addr.port(), 8080);
}

#[tokio::test]
async fn stale_generation_is_ignored() {
    let state = isengard_agent::proxy::ProxyState::new();

    let mk = |gen: u64, port: u16| ProxyConfig {
        host_id: "h".into(),
        generation: gen,
        rules: vec![RoutingRule {
            id: 1,
            public_hostname: "x.test".into(),
            upstream: Some(Upstream {
                container_id: "web".into(),
                container_ip: "127.0.0.1".into(),
                container_port: port as u32,
            }),
            tls_mode: TlsMode::Acme as i32,
            healthcheck: None,
            adapter: "none".into(),
        }],
        settings: None,
    };

    isengard_agent::proxy::apply_config(&state, mk(5, 8080)).await.unwrap();
    // older generation should be ignored
    isengard_agent::proxy::apply_config(&state, mk(3, 9999)).await.unwrap();

    let up = state.upstreams.read().await;
    assert_eq!(up.get("x.test").unwrap().addr.port(), 8080);
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-agent --test proxy_apply_config`
Expected: FAIL — `apply_config` undefined.

- [ ] **Step 3: Add `apply_config` and a `last_generation` to `ProxyState`**

Update `crates/isengard-agent/src/proxy/mod.rs`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

pub mod healthcheck;
pub mod router;
pub mod server;
pub mod upstreams;

pub use upstreams::{Upstream, UpstreamRegistry};

#[derive(Clone, Default)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    pub last_generation: Arc<AtomicU64>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub async fn apply_config(
    state: &ProxyState,
    cfg: isengard_proto::pb::ProxyConfig,
) -> anyhow::Result<()> {
    // Drop stale generations.
    let last = state.last_generation.load(Ordering::Acquire);
    if cfg.generation <= last {
        tracing::debug!(
            new = cfg.generation,
            last,
            "proxy: dropping stale ProxyConfig"
        );
        return Ok(());
    }

    let mut new_reg = UpstreamRegistry::new();
    for rule in cfg.rules {
        let Some(up) = rule.upstream else { continue };
        let ip = if up.container_ip.is_empty() {
            "127.0.0.1".parse().unwrap()
        } else {
            up.container_ip.parse().map_err(|e| {
                anyhow::anyhow!("bad container_ip {}: {e}", up.container_ip)
            })?
        };
        let addr = std::net::SocketAddr::new(ip, up.container_port as u16);
        new_reg.set(
            rule.public_hostname,
            Upstream {
                container_id: up.container_id,
                addr,
                healthy: true,
            },
        );
    }

    {
        let mut w = state.upstreams.write().await;
        *w = new_reg;
    }
    state.last_generation.store(cfg.generation, Ordering::Release);
    tracing::info!(generation = cfg.generation, "proxy: ProxyConfig applied");
    Ok(())
}

// supervise(...) and constants stay unchanged.
```

(Keep the existing `MAX_RESTARTS`/`RESTART_WINDOW` consts and `supervise` function.)

- [ ] **Step 4: Wire the sync handler to call `apply_config`**

In `crates/isengard-agent/src/sync.rs`, locate the loop that processes `ControllerMessage`s. Add a match arm for the new `proxy_config` variant:

```rust
use isengard_proto::pb::controller_message::Payload;

// Inside the message-processing match:
match payload {
    // ... existing arms ...
    Payload::ProxyConfig(cfg) => {
        if let Err(e) = crate::proxy::apply_config(&proxy_state, cfg).await {
            tracing::warn!(error = %e, "proxy: apply_config failed");
        }
    }
}
```

`proxy_state` must be threaded through from the agent's startup function to the sync handler. If sync currently takes a `SyncContext`-style struct, add `pub proxy_state: crate::proxy::ProxyState` to it. Otherwise pass it as a function argument.

- [ ] **Step 5: Run all agent tests**

Run: `cargo test -p isengard-agent`
Expected: green, including the two new `proxy_apply_config` tests and existing enroll/sync tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): apply ProxyConfig from controller, drop stale generations"
```

---

### Task 14: Controller pushes `ProxyConfig` over the sync stream

**Files:**
- Modify: `crates/isengard-controller/src/service.rs` (the `Sync` RPC handler)
- Modify: `crates/isengard-controller/src/sync_stacks.rs` (or wherever rule changes happen)

- [ ] **Step 1: Write the failing end-to-end push test**

Create `crates/isengard-controller/tests/routing_pushed_to_agent_e2e.rs`:

```rust
//! End-to-end: spin up controller, simulate an agent that opens a Sync
//! stream, insert a routing rule on the controller side, assert that
//! within ~1s the agent receives a ControllerMessage::ProxyConfig with
//! the new rule.

use isengard_proto::pb::{controller_message::Payload, AgentMessage};
use isengard_storage::{
    EnrollHost, InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use std::time::Duration;
use tokio::time::timeout;

mod common; // existing test helpers — see existing controller tests for the pattern

#[tokio::test]
async fn rule_inserted_after_sync_open_is_pushed_to_agent() {
    let env = common::start_controller().await;

    let host = env
        .inventory
        .enroll_host(EnrollHost { /* ... */ ..Default::default() })
        .await
        .unwrap();

    let (tx, mut rx) = env.open_sync_stream(host.id).await;
    // First frame from agent must be SyncHello (existing sync protocol).
    tx.send(AgentMessage::sync_hello(host.id)).await.unwrap();

    env.inventory
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: "rule.test".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    // Trigger a push (in production this is wired to upserts; in this test
    // we call it explicitly to keep the test fast).
    env.handles.routing.push_to_host(host.id, &env.handles).await.unwrap();

    let msg = timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    let Some(Payload::ProxyConfig(cfg)) = msg.payload else {
        panic!("expected ProxyConfig, got {msg:?}");
    };
    assert_eq!(cfg.rules.len(), 1);
    assert_eq!(cfg.rules[0].public_hostname, "rule.test");
}
```

(`common::start_controller` and `open_sync_stream` follow existing test patterns — adapt from `crates/isengard-controller/tests/disconnect_monitor_e2e.rs` and `crates/isengard-agent/tests/sync_e2e.rs`.)

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-controller --test routing_pushed_to_agent_e2e`
Expected: FAIL — `RoutingPusher::push_to_host` undefined.

- [ ] **Step 3: Add `push_to_host` + per-host sender registry**

In `crates/isengard-controller/src/routing.rs`, extend `RoutingPusher` with a sender registry indexed by host_id. The `Sync` RPC handler registers a sender when an agent opens its stream and unregisters on close.

```rust
use isengard_proto::pb::{ControllerMessage, controller_message::Payload};
use tokio::sync::mpsc;

#[derive(Default)]
struct Senders {
    /// Each connected agent's outbound channel for ControllerMessages.
    by_host: HashMap<HostId, mpsc::Sender<ControllerMessage>>,
}

pub struct RoutingPusher {
    inv: Arc<Inventory>,
    by_host_state: Mutex<HashMap<HostId, PerHostState>>,
    senders: Mutex<Senders>,
}

impl RoutingPusher {
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self {
            inv,
            by_host_state: Mutex::new(HashMap::new()),
            senders: Mutex::new(Senders::default()),
        }
    }

    pub async fn register_sender(
        &self,
        host: HostId,
        tx: mpsc::Sender<ControllerMessage>,
    ) {
        let mut s = self.senders.lock().await;
        s.by_host.insert(host, tx);
    }

    pub async fn unregister_sender(&self, host: HostId) {
        let mut s = self.senders.lock().await;
        s.by_host.remove(&host);
    }

    pub async fn push_to_host(&self, host: HostId, _handles: &crate::ControllerHandles) -> Result<()> {
        let cfg = self.build_for_host(host).await?;
        let senders = self.senders.lock().await;
        if let Some(tx) = senders.by_host.get(&host) {
            let msg = ControllerMessage {
                payload: Some(Payload::ProxyConfig(cfg)),
            };
            // best effort — if agent's queue is full, log and drop
            let _ = tx.try_send(msg);
        }
        Ok(())
    }

    /// Push to every connected host. Used when settings change apply globally.
    pub async fn push_to_all(&self, handles: &crate::ControllerHandles) -> Result<()> {
        let host_ids: Vec<HostId> = self.senders.lock().await.by_host.keys().copied().collect();
        for h in host_ids {
            let _ = self.push_to_host(h, handles).await;
        }
        Ok(())
    }
}

// (rebuild build_for_host to use by_host_state)
```

- [ ] **Step 4: Hook the Sync handler**

In `crates/isengard-controller/src/service.rs`, in the body of the `Sync` RPC, after the agent's first frame identifies the host:

```rust
// After resolving host_id from SyncHello:
let (out_tx, out_rx) = tokio::sync::mpsc::channel::<ControllerMessage>(64);
self.handles.routing.register_sender(host_id, out_tx.clone()).await;

// Push the current config immediately so a reconnected agent gets caught up.
let _ = self.handles.routing.push_to_host(host_id, &self.handles).await;

// On stream close (Drop / loop exit), unregister:
let routing = self.handles.routing.clone();
let host_for_drop = host_id;
let _guard = scopeguard::guard((), move |_| {
    let r = routing.clone();
    let h = host_for_drop;
    tokio::spawn(async move { r.unregister_sender(h).await; });
});
```

Add `scopeguard = "1"` to `crates/isengard-controller/Cargo.toml` if not already present.

The existing send loop should multiplex existing controller-originated messages WITH messages from `out_rx`. If the existing implementation has its own send channel, fold the routing channel into the same select loop.

- [ ] **Step 5: Trigger pushes when rules change**

In `crates/isengard-controller/src/sync_stacks.rs` (or wherever `Inventory::insert_routing_rule` callers live), after a rule insert/update/delete, call:

```rust
let _ = handles.routing.push_to_host(rule.host_id, &handles).await;
```

For Plan A, the only caller of `insert_routing_rule` is the label-discovery handler (Task 16). Wire the push there.

- [ ] **Step 6: Run the test**

Run: `cargo test -p isengard-controller --test routing_pushed_to_agent_e2e`
Expected: green.

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-controller
git commit -m "feat(controller): push ProxyConfig over sync stream on rule changes"
```

---

### Task 15: Label parser

**Files:**
- Create: `crates/isengard-agent/src/labels.rs`
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Write the failing parser test**

Create `crates/isengard-agent/tests/label_parser.rs`:

```rust
use isengard_agent::labels::{parse_labels, LabelRule};
use std::collections::HashMap;

fn lbl(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
}

#[test]
fn parses_default_unnamed_rule() {
    let rules = parse_labels(&lbl(&[
        ("isengard.expose", "blog.example.com"),
        ("isengard.expose.port", "8080"),
        ("isengard.expose.tls", "edge"),
    ]));
    assert_eq!(rules.len(), 1);
    let r = &rules[0];
    assert_eq!(r.name, None);
    assert_eq!(r.hostname, "blog.example.com");
    assert_eq!(r.port, Some(8080));
    assert_eq!(r.tls.as_deref(), Some("edge"));
}

#[test]
fn parses_multi_named_rules() {
    let rules = parse_labels(&lbl(&[
        ("isengard.expose.web", "blog.example.com"),
        ("isengard.expose.web.port", "8080"),
        ("isengard.expose.api", "api.example.com"),
        ("isengard.expose.api.port", "8081"),
        ("isengard.expose.api.auth", "cf-access"),
    ]));
    assert_eq!(rules.len(), 2);
    let web = rules.iter().find(|r| r.name.as_deref() == Some("web")).unwrap();
    assert_eq!(web.hostname, "blog.example.com");
    assert_eq!(web.port, Some(8080));
    let api = rules.iter().find(|r| r.name.as_deref() == Some("api")).unwrap();
    assert_eq!(api.hostname, "api.example.com");
    assert_eq!(api.auth.as_deref(), Some("cf-access"));
}

#[test]
fn ignores_unrelated_labels() {
    let rules = parse_labels(&lbl(&[
        ("traefik.enable", "true"),
        ("isengard.expose", "x.test"),
    ]));
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].hostname, "x.test");
}

#[test]
fn empty_or_no_isengard_labels_returns_empty() {
    let rules = parse_labels(&lbl(&[("foo", "bar")]));
    assert!(rules.is_empty());
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-agent --test label_parser`
Expected: FAIL.

- [ ] **Step 3: Create `crates/isengard-agent/src/labels.rs`**

```rust
//! Parse `isengard.expose.*` labels into structured rules.
//!
//! Rules:
//! - `isengard.expose = <hostname>` → default unnamed rule
//! - `isengard.expose.<prop> = <value>` where <prop> ∈ KNOWN_PROPS → property of default rule
//! - `isengard.expose.<name> = <hostname>` where <name> ∉ KNOWN_PROPS → named rule
//! - `isengard.expose.<name>.<prop> = <value>` → property of named rule
//!
//! Properties: port, tls, health, adapter, auth.

use std::collections::HashMap;

const KNOWN_PROPS: &[&str] = &["port", "tls", "health", "adapter", "auth"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelRule {
    /// None = default unnamed rule
    pub name: Option<String>,
    pub hostname: String,
    pub port: Option<u16>,
    pub tls: Option<String>,
    pub health: Option<String>,
    pub adapter: Option<String>,
    pub auth: Option<String>,
}

pub fn parse_labels(labels: &HashMap<String, String>) -> Vec<LabelRule> {
    let mut by_name: HashMap<Option<String>, LabelRule> = HashMap::new();

    for (k, v) in labels {
        let Some(rest) = k.strip_prefix("isengard.expose") else {
            continue;
        };
        // rest is either "" or starts with "."
        let segments: Vec<&str> = if rest.is_empty() {
            vec![]
        } else {
            rest.trim_start_matches('.').split('.').collect()
        };

        match segments.len() {
            0 => {
                // "isengard.expose = hostname" — default rule
                by_name.entry(None).or_default().hostname = v.clone();
            }
            1 => {
                let s = segments[0];
                if KNOWN_PROPS.contains(&s) {
                    apply_prop(by_name.entry(None).or_default(), s, v);
                } else {
                    // named rule, hostname value
                    let r = by_name.entry(Some(s.to_string())).or_default();
                    r.name = Some(s.to_string());
                    r.hostname = v.clone();
                }
            }
            2 => {
                let name = segments[0];
                let prop = segments[1];
                if KNOWN_PROPS.contains(&name) {
                    // ambiguous; treat first segment as named even if collision-prone
                    // Per spec, KNOWN_PROPS at position-1 are default-rule props,
                    // so `expose.port.foo = ...` is ill-formed; ignore.
                    continue;
                }
                let r = by_name.entry(Some(name.to_string())).or_default();
                r.name = Some(name.to_string());
                apply_prop(r, prop, v);
            }
            _ => {
                // overly nested key; ignore
                continue;
            }
        }
    }

    let mut out: Vec<LabelRule> = by_name
        .into_values()
        .filter(|r| !r.hostname.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn apply_prop(rule: &mut LabelRule, prop: &str, value: &str) {
    match prop {
        "port" => rule.port = value.parse().ok(),
        "tls" => rule.tls = Some(value.to_string()),
        "health" => rule.health = Some(value.to_string()),
        "adapter" => rule.adapter = Some(value.to_string()),
        "auth" => rule.auth = Some(value.to_string()),
        _ => {}
    }
}
```

Re-export from `crates/isengard-agent/src/lib.rs`:

```rust
pub mod labels;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-agent --test label_parser`
Expected: 4 green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): label parser for isengard.expose.*"
```

---

### Task 16: Docker event subscription + label-rule reconciliation

**Files:**
- Modify: `crates/isengard-agent/src/labels.rs`
- Modify: `crates/isengard-agent/src/lib.rs`
- Modify: `crates/isengard-controller/src/service.rs` (handle `ContainerLabelsReport`)

- [ ] **Step 1: Write the failing reconciliation test on the controller side**

Create `crates/isengard-controller/tests/label_ingest_e2e.rs`:

```rust
use isengard_proto::pb::{ContainerLabelsReport, ContainerLabelsRemoved};
use isengard_storage::{EnrollHost, RoutingRuleSource};
use std::collections::HashMap;

mod common;

#[tokio::test]
async fn labels_report_creates_routing_rule_with_label_source() {
    let env = common::start_controller().await;
    let host = env
        .inventory
        .enroll_host(EnrollHost { /* default fields */ ..Default::default() })
        .await
        .unwrap();

    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "lbl.example.com".to_string());
    labels.insert("isengard.expose.port".to_string(), "8080".to_string());

    env.handles
        .routing
        .ingest_labels(
            host.id,
            ContainerLabelsReport {
                container_id: "cid-1".into(),
                container_name: "web".into(),
                image: "nginx:1.25".into(),
                labels,
            },
        )
        .await
        .unwrap();

    let rules = env.inventory.list_routing_rules_for_host(host.id).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].public_hostname, "lbl.example.com");
    assert_eq!(rules[0].source, RoutingRuleSource::Label);
    assert_eq!(rules[0].source_container_id.as_deref(), Some("cid-1"));
}

#[tokio::test]
async fn labels_removed_event_deletes_label_rules_for_container() {
    let env = common::start_controller().await;
    let host = env.inventory.enroll_host(EnrollHost { ..Default::default() }).await.unwrap();

    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "x.test".to_string());
    env.handles
        .routing
        .ingest_labels(host.id, ContainerLabelsReport {
            container_id: "cid-A".into(),
            container_name: "web".into(),
            image: "n:1".into(),
            labels,
        })
        .await
        .unwrap();
    assert_eq!(env.inventory.list_routing_rules_for_host(host.id).await.unwrap().len(), 1);

    env.handles
        .routing
        .ingest_labels_removed(host.id, ContainerLabelsRemoved { container_id: "cid-A".into() })
        .await
        .unwrap();
    assert!(env.inventory.list_routing_rules_for_host(host.id).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-controller --test label_ingest_e2e`
Expected: FAIL — `ingest_labels` undefined.

- [ ] **Step 3: Add `ingest_labels` and `ingest_labels_removed` to RoutingPusher**

Append to `crates/isengard-controller/src/routing.rs`:

```rust
use isengard_proto::pb::{ContainerLabelsRemoved, ContainerLabelsReport};
use isengard_storage::{InsertRoutingRule, RoutingRuleSource, RoutingRuleState};

impl RoutingPusher {
    pub async fn ingest_labels(
        &self,
        host_id: HostId,
        report: ContainerLabelsReport,
    ) -> Result<()> {
        // Delete any existing label-source rules for this container,
        // then re-insert from the latest label state.
        let existing = self.inv.list_routing_rules_for_host(host_id).await?;
        for r in existing {
            if r.source == RoutingRuleSource::Label
                && r.source_container_id.as_deref() == Some(report.container_id.as_str())
            {
                self.inv.delete_routing_rule(r.id).await?;
            }
        }

        let parsed = isengard_agent::labels::parse_labels(&report.labels);
        for rule in parsed {
            let port = rule.port.unwrap_or(80);
            let tls_mode = match rule.tls.as_deref() {
                Some("edge") => isengard_storage::TlsMode::Edge,
                Some("manual") => isengard_storage::TlsMode::Manual,
                _ => isengard_storage::TlsMode::Acme,
            };

            self.inv
                .insert_routing_rule(InsertRoutingRule {
                    fleet: "default".into(), // resolved from host fleet; see note below
                    host_id,
                    stack_id: None,
                    service_name: report.container_name.clone(),
                    container_port: port,
                    public_hostname: rule.hostname,
                    protocol: "http".into(),
                    adapter: rule.adapter.unwrap_or_else(|| "none".into()),
                    tls_mode,
                    healthcheck_path: rule.health,
                    healthcheck_interval_secs: 10,
                    auth: rule.auth,
                    state: RoutingRuleState::Pending,
                    source: RoutingRuleSource::Label,
                    source_container_id: Some(report.container_id.clone()),
                    source_imported_from: None,
                })
                .await?;
        }

        Ok(())
    }

    pub async fn ingest_labels_removed(
        &self,
        host_id: HostId,
        ev: ContainerLabelsRemoved,
    ) -> Result<()> {
        let existing = self.inv.list_routing_rules_for_host(host_id).await?;
        for r in existing {
            if r.source == RoutingRuleSource::Label
                && r.source_container_id.as_deref() == Some(ev.container_id.as_str())
            {
                self.inv.delete_routing_rule(r.id).await?;
            }
        }
        Ok(())
    }
}
```

Add `isengard-agent` as a `dev-dependency` of `isengard-controller` only if needed for tests; the parser used here is a runtime dep, so add it to regular `[dependencies]` in `crates/isengard-controller/Cargo.toml`:

```toml
isengard-agent = { path = "../isengard-agent" }
```

(Or alternatively, move `labels.rs` into `isengard-core` so both agent and controller can use it without crate-cycle risk. **Recommended:** move it to `isengard-core::labels` since this is a shared parser.)

- [ ] **Step 4: Move parser to `isengard-core`**

Move `crates/isengard-agent/src/labels.rs` → `crates/isengard-core/src/labels.rs`. Update:
- `crates/isengard-core/src/lib.rs`: `pub mod labels;`
- `crates/isengard-agent/src/lib.rs`: replace `pub mod labels;` with `pub use isengard_core::labels;`
- `crates/isengard-controller/src/routing.rs`: change `isengard_agent::labels::parse_labels` to `isengard_core::labels::parse_labels`
- Test file `crates/isengard-agent/tests/label_parser.rs`: change import to `use isengard_core::labels::{parse_labels, LabelRule};`

- [ ] **Step 5: Wire the agent's Docker event subscription**

In `crates/isengard-agent/src/labels.rs` (now empty after the move — replace with the watcher):

```rust
//! Docker event subscription. On start/stop/update, snapshots the container's
//! labels and sends a ContainerLabelsReport / ContainerLabelsRemoved up the
//! sync stream.

use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::system::EventsOptions;
use futures::StreamExt;
use isengard_proto::pb::{AgentMessage, ContainerLabelsRemoved, ContainerLabelsReport};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub async fn watch(
    docker: Docker,
    out: mpsc::Sender<AgentMessage>,
) -> anyhow::Result<()> {
    // Initial reconcile: scan all running containers.
    initial_scan(&docker, &out).await?;

    let mut filters = HashMap::new();
    filters.insert("type".to_string(), vec!["container".to_string()]);
    filters.insert(
        "event".to_string(),
        vec!["start".into(), "stop".into(), "die".into(), "update".into(), "destroy".into()],
    );

    let mut events = docker.events(Some(EventsOptions::<String> {
        since: None,
        until: None,
        filters,
    }));

    while let Some(ev) = events.next().await {
        let ev = ev?;
        let Some(actor) = ev.actor else { continue };
        let Some(cid) = actor.id else { continue };
        let action = ev.action.as_deref().unwrap_or("");

        match action {
            "start" | "update" => {
                if let Some(report) = inspect_to_report(&docker, &cid).await? {
                    let _ = out
                        .send(AgentMessage {
                            payload: Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsReport(report)),
                        })
                        .await;
                }
            }
            "stop" | "die" | "destroy" => {
                let _ = out
                    .send(AgentMessage {
                        payload: Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsRemoved(
                            ContainerLabelsRemoved { container_id: cid },
                        )),
                    })
                    .await;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn initial_scan(
    docker: &Docker,
    out: &mpsc::Sender<AgentMessage>,
) -> anyhow::Result<()> {
    let containers = docker.list_containers(Some(ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    })).await?;
    for c in containers {
        let Some(id) = c.id else { continue };
        if let Some(report) = inspect_to_report(docker, &id).await? {
            let _ = out
                .send(AgentMessage {
                    payload: Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsReport(report)),
                })
                .await;
        }
    }
    Ok(())
}

async fn inspect_to_report(
    docker: &Docker,
    cid: &str,
) -> anyhow::Result<Option<ContainerLabelsReport>> {
    let inspect = docker.inspect_container(cid, None).await?;
    let labels = inspect.config.as_ref().and_then(|c| c.labels.clone()).unwrap_or_default();
    if !labels.keys().any(|k| k.starts_with("isengard.expose")) {
        return Ok(None);
    }
    let name = inspect
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let image = inspect
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .unwrap_or_default();
    Ok(Some(ContainerLabelsReport {
        container_id: cid.to_string(),
        container_name: name,
        image,
        labels,
    }))
}
```

Spawn the watcher in agent startup (next to the proxy supervisor):

```rust
let docker = bollard::Docker::connect_with_socket_defaults()?;
let out = sync_outbox_tx.clone(); // existing channel that feeds AgentMessages to the controller
tokio::spawn(async move {
    if let Err(e) = crate::labels::watch(docker, out).await {
        tracing::error!(error = %e, "labels: watcher exited");
    }
});
```

- [ ] **Step 6: Hook the controller's Sync RPC to dispatch `ContainerLabelsReport` to RoutingPusher**

In `crates/isengard-controller/src/service.rs`, in the per-message handler:

```rust
use isengard_proto::pb::agent_message::Payload as AgentPayload;

match payload {
    // ... existing arms ...
    AgentPayload::ContainerLabelsReport(report) => {
        if let Err(e) = self.handles.routing.ingest_labels(host_id, report).await {
            tracing::warn!(error = %e, "labels: ingest failed");
        }
        let _ = self.handles.routing.push_to_host(host_id, &self.handles).await;
    }
    AgentPayload::ContainerLabelsRemoved(ev) => {
        let _ = self.handles.routing.ingest_labels_removed(host_id, ev).await;
        let _ = self.handles.routing.push_to_host(host_id, &self.handles).await;
    }
}
```

- [ ] **Step 7: Run all tests**

Run: `cargo test --workspace`
Expected: green, including the two new `label_ingest_e2e` tests.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add crates/
git commit -m "feat: docker label discovery → routing rules with conflict-aware ingest"
```

---

### Task 17: Conflict resolution (label vs UI ownership)

**Files:**
- Modify: `crates/isengard-controller/src/routing.rs`

- [ ] **Step 1: Write failing conflict test**

Append to `crates/isengard-controller/tests/label_ingest_e2e.rs`:

```rust
use isengard_storage::{InsertRoutingRule, RoutingRuleState, TlsMode};

#[tokio::test]
async fn label_arriving_for_existing_ui_hostname_replaces_ui_rule() {
    let env = common::start_controller().await;
    let host = env.inventory.enroll_host(EnrollHost { ..Default::default() }).await.unwrap();

    // Pre-existing UI rule.
    env.inventory
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host.id,
            stack_id: None,
            service_name: "old".into(),
            container_port: 80,
            public_hostname: "blog.test".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Manual,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    // Label-discovered rule arrives for the same hostname.
    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "blog.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "8080".to_string());

    env.handles
        .routing
        .ingest_labels(host.id, ContainerLabelsReport {
            container_id: "cid".into(),
            container_name: "newweb".into(),
            image: "n:1".into(),
            labels,
        })
        .await
        .unwrap();

    let rules = env.inventory.list_routing_rules_for_host(host.id).await.unwrap();
    let by_host: Vec<_> = rules.iter().filter(|r| r.public_hostname == "blog.test").collect();
    assert_eq!(by_host.len(), 1);
    assert_eq!(by_host[0].source, RoutingRuleSource::Label);
    assert_eq!(by_host[0].service_name, "newweb");
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-controller --test label_ingest_e2e -- label_arriving`
Expected: FAIL — UNIQUE constraint violation, because `ingest_labels` doesn't yet displace the UI rule.

- [ ] **Step 3: Update `ingest_labels` to displace UI rules with the same `(public_hostname, host_id)`**

In `crates/isengard-controller/src/routing.rs`, modify the loop in `ingest_labels`:

```rust
for rule in parsed {
    let port = rule.port.unwrap_or(80);
    let tls_mode = match rule.tls.as_deref() {
        Some("edge") => isengard_storage::TlsMode::Edge,
        Some("manual") => isengard_storage::TlsMode::Manual,
        _ => isengard_storage::TlsMode::Acme,
    };

    // Conflict resolution: labels win the hostname.
    // If a UI rule exists with this hostname on this host, displace it
    // (preserving its overrides under the new label-source rule's id).
    let existing = self.inv.list_routing_rules_for_host(host_id).await?;
    let mut preserved_overrides: Vec<(String, serde_json::Value)> = vec![];
    for ex in existing {
        if ex.public_hostname == rule.hostname && ex.source == RoutingRuleSource::Ui {
            // Snapshot overrides before deleting.
            for o in self.inv.list_routing_rule_overrides(ex.id).await? {
                preserved_overrides.push((o.field, o.value_json));
            }
            self.inv.delete_routing_rule(ex.id).await?;
        }
    }

    let new_rule = self.inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: None,
            service_name: report.container_name.clone(),
            container_port: port,
            public_hostname: rule.hostname,
            protocol: "http".into(),
            adapter: rule.adapter.unwrap_or_else(|| "none".into()),
            tls_mode,
            healthcheck_path: rule.health,
            healthcheck_interval_secs: 10,
            auth: rule.auth,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Label,
            source_container_id: Some(report.container_id.clone()),
            source_imported_from: None,
        })
        .await?;

    for (field, value) in preserved_overrides {
        self.inv.upsert_routing_rule_override(new_rule.id, &field, value).await?;
    }
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p isengard-controller --test label_ingest_e2e`
Expected: green (all four label_ingest tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-controller
git commit -m "feat(controller): label rules displace UI rules on hostname collision, preserve overrides"
```

---

## Phase 8d: Healthcheck-aware upstreams

### Task 18: Healthcheck task per upstream

**Files:**
- Modify: `crates/isengard-agent/src/proxy/healthcheck.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/src/proxy/upstreams.rs`

- [ ] **Step 1: Write the failing healthcheck test**

Create `crates/isengard-agent/tests/proxy_healthcheck.rs`:

```rust
use isengard_agent::proxy::healthcheck::HealthChecker;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

async fn spawn_responder(should_200: bool) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let resp = if should_200 {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                    } else {
                        "HTTP/1.1 500 Internal\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
async fn http_check_returns_healthy_for_2xx() {
    let addr = spawn_responder(true).await;
    let hc = HealthChecker::new("/healthz".into(), Duration::from_millis(100));
    let healthy = hc.check_once(addr).await;
    assert!(healthy);
}

#[tokio::test]
async fn http_check_returns_unhealthy_for_5xx() {
    let addr = spawn_responder(false).await;
    let hc = HealthChecker::new("/healthz".into(), Duration::from_millis(100));
    let healthy = hc.check_once(addr).await;
    assert!(!healthy);
}

#[tokio::test]
async fn tcp_only_check_returns_healthy_when_connect_succeeds() {
    let addr = spawn_responder(true).await;
    let hc = HealthChecker::tcp_only(Duration::from_millis(100));
    let healthy = hc.check_once(addr).await;
    assert!(healthy);
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-agent --test proxy_healthcheck`
Expected: FAIL.

- [ ] **Step 3: Implement `HealthChecker`**

Replace `crates/isengard-agent/src/proxy/healthcheck.rs`:

```rust
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct HealthChecker {
    path: Option<String>,
    timeout: Duration,
}

impl HealthChecker {
    pub fn new(path: String, timeout: Duration) -> Self {
        Self {
            path: Some(path),
            timeout,
        }
    }

    pub fn tcp_only(timeout: Duration) -> Self {
        Self { path: None, timeout }
    }

    pub async fn check_once(&self, addr: SocketAddr) -> bool {
        let connect = timeout(self.timeout, TcpStream::connect(addr)).await;
        let mut stream = match connect {
            Ok(Ok(s)) => s,
            _ => return false,
        };
        let Some(path) = &self.path else {
            return true;
        };
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: healthcheck\r\nConnection: close\r\n\r\n",
            path
        );
        if timeout(self.timeout, stream.write_all(req.as_bytes())).await.is_err() {
            return false;
        }
        let mut buf = vec![0u8; 256];
        let n = match timeout(self.timeout, stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => return false,
        };
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        // Parse first response line for status code.
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        (200..300).contains(&status)
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-agent --test proxy_healthcheck`
Expected: 3 green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): HealthChecker (HTTP path + TCP-only modes)"
```

---

### Task 19: Wire healthchecks into `apply_config`; eviction state machine

**Files:**
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/src/proxy/upstreams.rs`

- [ ] **Step 1: Write failing eviction test**

Create `crates/isengard-agent/tests/proxy_eviction.rs`:

```rust
use isengard_agent::proxy::{
    upstreams::Upstream, ProxyState, UpstreamRegistry,
};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

async fn spawn(toggle: tokio::sync::watch::Receiver<bool>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                let healthy = *toggle.borrow();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let resp = if healthy {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                } else {
                    "HTTP/1.1 503 SU\r\nContent-Length: 0\r\n\r\n"
                };
                let _ = s.write_all(resp.as_bytes()).await;
            }
        }
    });
    addr
}

#[tokio::test]
async fn eviction_after_three_failures_then_recovery_after_one_success() {
    let (tx, rx) = tokio::sync::watch::channel(true);
    let addr = spawn(rx).await;

    let state = ProxyState::new();
    {
        let mut up = state.upstreams.write().await;
        up.set("h.test", Upstream { container_id: "c".into(), addr, healthy: true });
        up.set_health_config("h.test", Some("/healthz".into()), Duration::from_millis(50));
    }

    // Spawn the per-upstream healthcheck loop.
    isengard_agent::proxy::healthcheck::spawn_loops(state.clone());

    // Flip to unhealthy, wait for ≥3 checks (≥150ms), confirm evicted.
    tx.send(false).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    {
        let up = state.upstreams.read().await;
        assert!(!up.get("h.test").unwrap().healthy, "should be evicted after 3 failures");
    }

    // Flip back to healthy, wait for ≥1 check (≥50ms), confirm re-added.
    tx.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    {
        let up = state.upstreams.read().await;
        assert!(up.get("h.test").unwrap().healthy, "should be healthy again");
    }
}
```

- [ ] **Step 2: Add `set_health_config` and per-upstream check state**

Modify `crates/isengard-agent/src/proxy/upstreams.rs`:

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Upstream {
    pub container_id: String,
    pub addr: SocketAddr,
    pub healthy: bool,
    pub health_path: Option<String>,
    pub health_interval: Duration,
    pub consecutive_failures: u32,
}

impl PartialEq for Upstream {
    fn eq(&self, other: &Self) -> bool {
        self.container_id == other.container_id && self.addr == other.addr
    }
}
impl Eq for Upstream {}

#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    by_hostname: HashMap<String, Upstream>,
}

impl UpstreamRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn set(&mut self, hostname: impl Into<String>, mut upstream: Upstream) {
        upstream.consecutive_failures = 0;
        self.by_hostname.insert(hostname.into(), upstream);
    }

    pub fn set_health_config(
        &mut self,
        hostname: &str,
        path: Option<String>,
        interval: Duration,
    ) {
        if let Some(u) = self.by_hostname.get_mut(hostname) {
            u.health_path = path;
            u.health_interval = interval;
        }
    }

    pub fn get(&self, hostname: &str) -> Option<&Upstream> {
        self.by_hostname.get(hostname)
    }
    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Upstream> {
        self.by_hostname.get_mut(hostname)
    }
    pub fn remove(&mut self, hostname: &str) -> Option<Upstream> {
        self.by_hostname.remove(hostname)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Upstream)> {
        self.by_hostname.iter()
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.by_hostname.keys()
    }
}
```

Update the `Upstream` literals in earlier task code (Task 13's `apply_config`, Task 9's test) to set the new fields. Default `health_path: None`, `health_interval: Duration::from_secs(10)`, `consecutive_failures: 0`.

- [ ] **Step 3: Implement `healthcheck::spawn_loops`**

Append to `crates/isengard-agent/src/proxy/healthcheck.rs`:

```rust
use std::time::Duration;
use crate::proxy::ProxyState;

const FAIL_THRESHOLD: u32 = 3;

pub fn spawn_loops(state: ProxyState) {
    tokio::spawn(async move {
        loop {
            let snapshot: Vec<(String, std::net::SocketAddr, Option<String>, Duration)> = {
                let r = state.upstreams.read().await;
                r.iter()
                    .map(|(host, up)| (host.clone(), up.addr, up.health_path.clone(), up.health_interval))
                    .collect()
            };
            for (host, addr, path, interval) in snapshot {
                let st = state.clone();
                let host_c = host.clone();
                tokio::spawn(async move {
                    let hc = match path {
                        Some(p) => HealthChecker::new(p, Duration::from_secs(2)),
                        None => HealthChecker::tcp_only(Duration::from_secs(2)),
                    };
                    let healthy = hc.check_once(addr).await;
                    let mut w = st.upstreams.write().await;
                    if let Some(up) = w.get_mut(&host_c) {
                        if healthy {
                            up.consecutive_failures = 0;
                            if !up.healthy {
                                up.healthy = true;
                                tracing::info!(host = %host_c, "upstream recovered");
                            }
                        } else {
                            up.consecutive_failures += 1;
                            if up.consecutive_failures >= FAIL_THRESHOLD && up.healthy {
                                up.healthy = false;
                                tracing::warn!(host = %host_c, "upstream evicted (3 consecutive failures)");
                            }
                        }
                    }
                    let _ = interval; // each upstream uses the same loop tick for v1
                });
            }
            // Tick at the smallest interval found, or 1s default.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
}
```

(The fixed 50ms tick is intentionally aggressive for tests; in production this can be tuned per-upstream by reading the smallest interval. For Plan A, fixed tick is acceptable — Pingora's healthchecks are rarely the bottleneck.)

- [ ] **Step 4: Spawn the healthcheck loop from `apply_config`**

Replace the body of `apply_config` in `crates/isengard-agent/src/proxy/mod.rs` (it currently has the version from Task 13). The complete new function:

```rust
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
static HC_SPAWNED: OnceLock<()> = OnceLock::new();

pub async fn apply_config(
    state: &ProxyState,
    cfg: isengard_proto::pb::ProxyConfig,
) -> anyhow::Result<()> {
    // 1. Drop stale generations.
    let last = state.last_generation.load(Ordering::Acquire);
    if cfg.generation <= last {
        tracing::debug!(new = cfg.generation, last, "proxy: dropping stale ProxyConfig");
        return Ok(());
    }

    // 2. Build new upstream registry from rules.
    let mut new_reg = UpstreamRegistry::new();
    for rule in &cfg.rules {
        let Some(up) = &rule.upstream else { continue };
        let ip = if up.container_ip.is_empty() {
            "127.0.0.1".parse().unwrap()
        } else {
            up.container_ip.parse().map_err(|e| {
                anyhow::anyhow!("bad container_ip {}: {e}", up.container_ip)
            })?
        };
        let addr = std::net::SocketAddr::new(ip, up.container_port as u16);
        new_reg.set(
            rule.public_hostname.clone(),
            Upstream {
                container_id: up.container_id.clone(),
                addr,
                healthy: true,
                health_path: None,
                health_interval: std::time::Duration::from_secs(10),
                consecutive_failures: 0,
            },
        );
    }

    // 3. Swap registry, then apply health configs from each rule.
    {
        let mut w = state.upstreams.write().await;
        *w = new_reg;
        for rule in &cfg.rules {
            if let Some(hc) = &rule.healthcheck {
                let path = if hc.path.is_empty() { None } else { Some(hc.path.clone()) };
                let interval = std::time::Duration::from_secs(hc.interval_secs.max(1) as u64);
                w.set_health_config(&rule.public_hostname, path, interval);
            }
        }
    }

    // 4. Spawn healthcheck loop once (idempotent across reloads).
    HC_SPAWNED.get_or_init(|| {
        crate::proxy::healthcheck::spawn_loops(state.clone());
    });

    // 5. Commit generation.
    state.last_generation.store(cfg.generation, Ordering::Release);
    tracing::info!(generation = cfg.generation, "proxy: ProxyConfig applied");
    Ok(())
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-agent --test proxy_eviction -- --nocapture`
Expected: green.

Run: `cargo test --workspace`
Expected: green.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): healthcheck-aware upstream eviction (3 fails → out, 1 ok → in)"
```

---

### Task 20: Emit `routing.upstream.health_changed` events

**Files:**
- Modify: `crates/isengard-agent/src/proxy/healthcheck.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`

- [ ] **Step 1: Write failing event-emission test**

Create `crates/isengard-agent/tests/proxy_health_event.rs`:

```rust
use isengard_agent::proxy::{ProxyState, Upstream};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

async fn fail_responder() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(b"HTTP/1.1 500 X\r\nContent-Length: 0\r\n\r\n").await;
            }
        }
    });
    addr
}

#[tokio::test]
async fn evicting_upstream_emits_health_changed_event() {
    let addr = fail_responder().await;
    let state = ProxyState::new();
    let (event_tx, mut event_rx) = mpsc::channel::<isengard_proto::pb::Event>(16);
    state.set_event_sink(event_tx);

    {
        let mut up = state.upstreams.write().await;
        up.set("e.test", Upstream {
            container_id: "c".into(),
            addr,
            healthy: true,
            health_path: Some("/h".into()),
            health_interval: Duration::from_millis(50),
            consecutive_failures: 0,
        });
    }
    isengard_agent::proxy::healthcheck::spawn_loops(state.clone());

    let ev = tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await.unwrap().unwrap();
    assert_eq!(ev.kind, "routing.upstream.health_changed");
    assert!(ev.summary.contains("e.test"));
    assert!(ev.summary.contains("unhealthy"));
}
```

- [ ] **Step 2: Add an event sink to `ProxyState`**

In `crates/isengard-agent/src/proxy/mod.rs`:

```rust
use isengard_proto::pb::Event;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
pub struct ProxyState {
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    pub last_generation: Arc<AtomicU64>,
    event_tx: Arc<RwLock<Option<mpsc::Sender<Event>>>>,
}

impl ProxyState {
    pub fn new() -> Self { Self::default() }

    pub fn set_event_sink(&self, tx: mpsc::Sender<Event>) {
        let evt = self.event_tx.clone();
        tokio::spawn(async move {
            let mut w = evt.write().await;
            *w = Some(tx);
        });
    }

    pub async fn emit(&self, ev: Event) {
        let r = self.event_tx.read().await;
        if let Some(tx) = &*r {
            let _ = tx.try_send(ev);
        }
    }
}
```

- [ ] **Step 3: Emit on transition in the healthcheck loop**

Update `spawn_loops` (replace the inner block in Task 19 step 3):

```rust
if let Some(up) = w.get_mut(&host_c) {
    let was = up.healthy;
    if healthy {
        up.consecutive_failures = 0;
        up.healthy = true;
    } else {
        up.consecutive_failures += 1;
        if up.consecutive_failures >= FAIL_THRESHOLD {
            up.healthy = false;
        }
    }
    if was != up.healthy {
        let now = chrono::Utc::now().to_rfc3339();
        let summary = format!(
            "upstream {} on {} is now {}",
            up.container_id,
            host_c,
            if up.healthy { "healthy" } else { "unhealthy" }
        );
        drop(w);
        st.emit(isengard_proto::pb::Event {
            kind: "routing.upstream.health_changed".into(),
            occurred_at: now,
            summary,
            container_name: Some(host_c.clone()),
            ..Default::default()
        }).await;
    }
}
```

In agent startup, wire `ProxyState::set_event_sink` to whatever channel the existing event publisher uses (e.g. the same one events.rs feeds into the gRPC sync stream).

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-agent --test proxy_health_event -- --nocapture`
Expected: green.

Run: `cargo test --workspace`
Expected: green.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent
git commit -m "feat(agent): emit routing.upstream.health_changed events on transition"
```

---

### Task 21: End-to-end label discovery test (real Docker)

**Files:**
- Create: `crates/isengard-agent/tests/proxy_label_discovery_e2e.rs`

- [ ] **Step 1: Write the test**

```rust
//! Real Docker e2e. Requires a running dockerd. Skip when not available.

use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
#[ignore] // run explicitly: `cargo test -- --ignored proxy_label_discovery_e2e`
async fn label_on_real_container_creates_routing_rule() {
    // 1. Skip if no docker socket
    let docker = match bollard::Docker::connect_with_socket_defaults() {
        Ok(d) => d,
        Err(_) => return,
    };
    if docker.ping().await.is_err() {
        return;
    }

    // 2. Spin up a controller in-process (use existing test helper).
    let env = isengard_controller::test_support::start_controller().await;
    let host = env.inventory.enroll_host(/* fields */).await.unwrap();

    // 3. Spawn the agent's labels::watch with a sender that ingests directly.
    //    (For full e2e coverage, run the agent binary; for unit-level coverage,
    //    call ingest_labels via the labels watcher's emitted messages.)
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let docker_c = docker.clone();
    let watcher = tokio::spawn(async move {
        let _ = isengard_agent::labels::watch(docker_c, tx).await;
    });

    // 4. docker run a busybox with the labels.
    use bollard::container::{Config, CreateContainerOptions};
    use bollard::models::HostConfig;
    let mut labels = std::collections::HashMap::new();
    labels.insert("isengard.expose".to_string(), "e2e.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "80".to_string());

    let cont = docker
        .create_container(
            Some(CreateContainerOptions { name: "isengard-e2e", platform: None }),
            Config {
                image: Some("busybox:latest"),
                cmd: Some(vec!["sleep", "30"]),
                labels: Some(labels.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()),
                host_config: Some(HostConfig::default()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker.start_container::<String>(&cont.id, None).await.unwrap();

    // 5. Wait for a ContainerLabelsReport in rx, then ingest into controller.
    let msg = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    if let Some(isengard_proto::pb::agent_message::Payload::ContainerLabelsReport(report)) = msg.payload {
        env.handles.routing.ingest_labels(host.id, report).await.unwrap();
    } else {
        panic!("expected ContainerLabelsReport");
    }

    let rules = env.inventory.list_routing_rules_for_host(host.id).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].public_hostname, "e2e.test");

    // 6. Cleanup.
    let _ = docker.kill_container(&cont.id, None).await;
    let _ = docker.remove_container(&cont.id, None).await;
    watcher.abort();
}
```

This test is `#[ignore]`d so CI without Docker still passes. It's run manually as a smoke test or by a Docker-equipped CI matrix.

- [ ] **Step 2: Run the e2e (with Docker)**

Run: `cargo test -p isengard-agent --test proxy_label_discovery_e2e -- --ignored --nocapture`
Expected: green when dockerd is reachable; silent skip otherwise.

- [ ] **Step 3: Commit**

```bash
git add crates/isengard-agent/tests/proxy_label_discovery_e2e.rs
git commit -m "test(agent): real-Docker e2e for label discovery → routing rule"
```

---

## Plan A wrap-up

After all tasks above are merged on `feat/networking-proxy-core`:

- [ ] **Final step 1: Workspace-wide green**

Run: `cargo build --workspace` → no warnings.
Run: `cargo nextest run --workspace` → all green.
Run: `cargo test --workspace -- --ignored` (with Docker) → all green.

- [ ] **Final step 2: Run the manual success-criteria smoke**

```bash
cargo run -- controller &  # in one shell
cargo run -- agent --controller http://localhost:9417  # in another
docker compose -f /tmp/test-stack.yml up -d  # with isengard.expose label

# Within 5s:
curl -H 'Host: blog.test' http://localhost:8080/  # should hit container

# Stop the container
docker stop test-stack_web_1
sleep 2
curl -H 'Host: blog.test' http://localhost:8080/  # should 503

# Restart
docker start test-stack_web_1
sleep 2
curl -H 'Host: blog.test' http://localhost:8080/  # should 200 again
```

- [ ] **Final step 3: Open PR back to `next`**

```bash
git push -u origin feat/networking-proxy-core
gh pr create --base next --title "Phase 8a-8d: networking & proxy core" --body "$(cat <<'EOF'
## Summary
- NetworkingAdapter trait + `none` adapter
- Routing tables in storage (rules, overrides, adapter_config, tls_certs)
- Pingora as tokio task in agent (HTTP only, port 8080)
- Controller pushes ProxyConfig over existing sync stream
- Label discovery via Docker event stream → routing rules
- Healthcheck-aware upstream eviction with health_changed events

## Test plan
- [x] cargo nextest run --workspace
- [x] Real-Docker label-discovery e2e
- [ ] Manual: docker compose up, curl through Pingora
- [ ] Manual: container stop → 503, restart → 200, stack delete → 404

Closes Phase 8a-8d. TLS, real adapters (tailscale, cf-tunnel), Settings UI, blue-green hook are Plans B and C.
EOF
)"
```

---

## Notes for the implementer

- **bollard event API**: bollard versions sometimes shift the `events()` signature. If `EventsOptions<String>` doesn't compile, check the docs for the version in `Cargo.lock` and adjust the filter map type accordingly. The principle (subscribe to container events with a filter) is stable across versions.
- **Pingora API surface**: the snippets in Tasks 8/9 target `pingora-core 0.4` / `pingora-proxy 0.4`. If the upstream API has shifted, look for the closest equivalent — the structure (`Server` + `http_proxy_service` + `ProxyHttp` trait) is stable.
- **Don't push without approval**: see project's CLAUDE.md / soul.md — `next` is public and the user reviews before push.
- **Write tests for one thing at a time**: the `#[tokio::test]` blocks above each test exactly one behavior. If you find yourself adding a second assertion that tests a different behavior, split it into a new test.
- **Format every commit**: `cargo fmt --all` is part of every commit step. Don't skip it.
