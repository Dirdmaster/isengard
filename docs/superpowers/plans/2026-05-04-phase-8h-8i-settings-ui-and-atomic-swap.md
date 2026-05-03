# Phase 8 Plan C: Settings UI Networking Tab + Atomic Upstream Swap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the Networking tab in the dashboard (CRUD for routing rules + adapter config) and the internal `swap_upstream` API on the agent for blue-green's drain-then-replace primitive.

**Architecture:** Backend gains REST endpoints in the dashboard plugin (`api/routing.rs`) backed by the existing `RoutingPusher` + `Inventory` storage from Plan A. Frontend converts the single-page `/settings` to tabs, adds a Networking tab with adapter cards + routing rules table + 2 modals. Agent gains an atomic `swap_upstream(hostname, new_upstream, grace_period)` with Active/Draining/Removed state on `Upstream`, and `IsengardProxy::upstream_peer` skips draining entries.

**Tech Stack:** Rust 2024, axum (existing in dashboard plugin), Vue 3 / Nuxt 3 / Pinia / TypeScript composables (existing). No new dependencies.

**Branch:** `feat/networking-settings-ui-and-swap` off `feat/networking-tls-adapters` (in worktree `.worktrees/networking-settings-ui-and-swap`). Do NOT push without explicit approval — `next` is public.

**Spec:** `docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`

**Predecessors:** Plan A (PR #18, branch `feat/networking-proxy-core`), Plan B (PR #19, branch `feat/networking-tls-adapters`).

---

## Scope

In:
- 8h-1: REST endpoints in `dashboard/src/api/routing.rs` (rules CRUD + overrides + adapter config + adapter test)
- 8h-2: `SettingsTabs.vue` + refactor existing settings sections (general/notifier/enrollment) into tab views
- 8h-3: `NetworkingSettings.vue` + `RoutingRulesTable.vue` + `useRoutingRules.ts`
- 8h-4: `RoutingRuleEditModal.vue` + `ServiceExposeModal.vue`
- 8h-5: 3 adapter cards (`AdapterCardCfTunnel`, `AdapterCardTailscale`, `AdapterCardNone`) + `useAdapterConfig.ts`
- 8i-1: `Upstream.state: UpstreamState` field + `UpstreamRegistry` methods
- 8i-2: `proxy/swap.rs` + `swap_upstream` function + router skip-draining guard + 3 unit tests

Out:
- Phase 9 (Update Policies) — separate plan
- Phase 10 (Blue-Green) — uses swap_upstream but separate plan
- DNS-01 ACME, bulk import, mTLS to upstream, HTTP/3, custom-domain tailscale, etc. — all later
- Vue component test runner setup — separate cleanup plan

Done when:
1. `cargo build --workspace` clean, no warnings under `-D warnings`
2. `cargo test --workspace` passes (including new HTTP integration tests + swap unit tests)
3. `cargo clippy --workspace --all-targets -- -D warnings` clean
4. `cargo deny check` clean
5. `bun run build` in `dashboard/web/` succeeds (the dashboard plugin's `build.rs` runs this)
6. Manual smoke (in PR body): `/settings?tab=networking` renders; create/edit/delete a UI-source rule; configure cf-tunnel adapter; "Test connection" returns sensible result
7. ~16-18 commits on `feat/networking-settings-ui-and-swap`, each green individually

---

## File Structure

### Create

| File | Responsibility |
|---|---|
| `crates/isengard-plugins/dashboard/src/api/mod.rs` | New: re-export the new `routing` submodule (if `api.rs` is currently a single file, this task migrates it to `api/mod.rs` + per-resource files only as needed) |
| `crates/isengard-plugins/dashboard/src/api/routing.rs` | REST handlers for `/api/routing/*` and `/api/networking/adapter-config/*` |
| `crates/isengard-plugins/dashboard/tests/api_routing.rs` | Integration tests against the new HTTP endpoints |
| `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue` | Tab container + URL query-param binding |
| `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue` | Networking tab body |
| `crates/isengard-plugins/dashboard/web/components/RoutingRulesTable.vue` | Table with SOURCE column + actions menu |
| `crates/isengard-plugins/dashboard/web/components/RoutingRuleEditModal.vue` | Create/edit modal |
| `crates/isengard-plugins/dashboard/web/components/ServiceExposeModal.vue` | Modal opened from Stack detail's Expose button |
| `crates/isengard-plugins/dashboard/web/components/AdapterCardNone.vue` | none adapter card (minimal) |
| `crates/isengard-plugins/dashboard/web/components/AdapterCardTailscale.vue` | tailscale adapter card |
| `crates/isengard-plugins/dashboard/web/components/AdapterCardCfTunnel.vue` | cf-tunnel adapter card |
| `crates/isengard-plugins/dashboard/web/composables/useRoutingRules.ts` | Fetch/mutate routing rules |
| `crates/isengard-plugins/dashboard/web/composables/useAdapterConfig.ts` | Fetch/mutate adapter config + test connection |
| `crates/isengard-agent/src/proxy/swap.rs` | `swap_upstream` function with drain semantics |
| `crates/isengard-agent/tests/proxy_swap_unit.rs` | 3 unit tests for state transitions + drain timing + router guard |

### Modify

| File | Change |
|---|---|
| `crates/isengard-plugins/dashboard/src/api.rs` | Either rename to `api/mod.rs` and split out `routing.rs`, OR keep flat and add `pub mod routing;`-equivalent. Plan picks the latter for minimal churn — see Task 1 |
| `crates/isengard-plugins/dashboard/src/lib.rs` | Wire the new `routing` axum router into the existing app |
| `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` | Replace single-page layout with `<SettingsTabs>` |
| `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue` | Wire ServiceExposeModal onto the existing "Expose" button |
| `crates/isengard-agent/src/proxy/upstreams.rs` | `Upstream.state: UpstreamState` field + `UpstreamRegistry::set_state` + `remove_if_draining` |
| `crates/isengard-agent/src/proxy/router.rs` | `IsengardProxy::upstream_peer` skips `Draining` upstreams |
| `crates/isengard-agent/src/proxy/mod.rs` | `pub mod swap; pub use swap::swap_upstream;` |

---

## Phase 8h-1: REST endpoints in dashboard plugin

### Task 1: Add `routing.rs` module + scaffold the new endpoints (no logic yet)

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/api/routing.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs`

This task scaffolds the module + wires it into the axum router. Endpoints return 501 Not Implemented for now; Tasks 2-5 fill them in one resource at a time.

- [ ] **Step 1: Read the existing API surface to confirm the router pattern**

Read `crates/isengard-plugins/dashboard/src/api.rs` and `crates/isengard-plugins/dashboard/src/lib.rs` to see:
- How the existing axum `Router` is composed
- How handler functions take `State<...>` (probably `State<Arc<ControllerHandles>>`)
- What auth middleware wraps the routes

Match that pattern. **Do NOT split `api.rs` into a directory** — keep it flat. Add `pub mod routing;` (which lives at `api/routing.rs`) and merge the routing router into the existing app.

(If `crates/isengard-plugins/dashboard/src/api/` already exists as a directory, add `routing.rs` to it.)

- [ ] **Step 2: Create the scaffold**

Create `crates/isengard-plugins/dashboard/src/api/routing.rs` (or `crates/isengard-plugins/dashboard/src/routing.rs` if `api/` is flat — match the actual pattern):

```rust
//! REST endpoints for routing rules + adapter config (Phase 8h Plan C).
//! See spec §3 in
//! `docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post, put};
use axum::Router;
use std::sync::Arc;

use crate::ControllerHandles; // adapt to actual type name from lib.rs

pub fn router() -> Router<Arc<ControllerHandles>> {
    Router::new()
        // Routing rules
        .route("/routing/rules", get(list_rules).post(create_rule))
        .route("/routing/rules/:id", axum::routing::patch(update_rule).delete(delete_rule))
        .route("/routing/rules/:id/overrides", get(list_overrides))
        .route("/routing/rules/:id/overrides/:field", put(upsert_override))
        // Adapter config
        .route(
            "/networking/adapter-config/:host_id/:adapter",
            get(get_adapter_config).put(upsert_adapter_config),
        )
        .route(
            "/networking/adapter-config/:host_id/:adapter/test",
            post(test_adapter_config),
        )
}

// === Stubs (filled in by tasks 2-5) ===

async fn list_rules(_state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "list_rules: 8h-1 task 2").into_response()
}
async fn create_rule(_state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "create_rule: 8h-1 task 2").into_response()
}
async fn update_rule(_p: Path<i64>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "update_rule: 8h-1 task 2").into_response()
}
async fn delete_rule(_p: Path<i64>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "delete_rule: 8h-1 task 2").into_response()
}
async fn list_overrides(_p: Path<i64>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "list_overrides: 8h-1 task 3").into_response()
}
async fn upsert_override(_p: Path<(i64, String)>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "upsert_override: 8h-1 task 3").into_response()
}
async fn get_adapter_config(_p: Path<(String, String)>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "get_adapter_config: 8h-1 task 4").into_response()
}
async fn upsert_adapter_config(_p: Path<(String, String)>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "upsert_adapter_config: 8h-1 task 4").into_response()
}
async fn test_adapter_config(_p: Path<(String, String)>, _state: State<Arc<ControllerHandles>>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "test_adapter_config: 8h-1 task 5").into_response()
}
```

- [ ] **Step 3: Wire into `lib.rs`**

In `crates/isengard-plugins/dashboard/src/lib.rs`, find where the axum `Router` is built (likely a `build_router` or inline in the plugin's start method). Merge the new router under `/api`:

```rust
use crate::api::routing as routing_api;
// existing router building code...
let app = app.merge(routing_api::router().route_layer(/* existing auth middleware */));
```

Match whatever auth-middleware-application pattern is already in place for other `/api/*` routes.

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p isengard-plugin-dashboard`
Expected: success.

Run: `cargo test --workspace`
Expected: green (no new tests yet; existing tests still pass).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/src/
git commit -m "feat(dashboard): scaffold routing/adapter-config API endpoints (501 stubs)"
```

---

### Task 2: Implement `routing/rules` CRUD endpoints (list/create/update/delete)

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/api/routing.rs`
- Create: `crates/isengard-plugins/dashboard/tests/api_routing.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/isengard-plugins/dashboard/tests/api_routing.rs`:

```rust
//! Integration tests for /api/routing/rules and /api/networking/adapter-config.
//!
//! Uses the existing dashboard test scaffolding pattern. Look at any other
//! `tests/api_*.rs` in this crate for the shape if this is your first touch.

use axum_test::TestServer;
use isengard_plugin_dashboard::api::routing::router as routing_router;
use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use std::sync::Arc;
use tempfile::tempdir;

async fn seed_inventory_with_one_rule() -> (Arc<Inventory>, isengard_storage::HostId, i64) {
    let dir = tempdir().unwrap();
    let inv = Arc::new(Inventory::open(&dir.path().join("isengard.db")).await.unwrap());
    let host = inv.enroll_host(EnrollHost {
        fingerprint: "fp".into(), hostname: "h".into(), os: "linux".into(),
        arch: "x86_64".into(), agent_version: "0".into(), docker_version: "27".into(),
        fleet: "default".into(),
    }).await.unwrap();
    let r = inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id: host,
        stack_id: None,
        service_name: "web".into(),
        container_port: 80,
        public_hostname: "blog.example.com".into(),
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
    }).await.unwrap();
    (inv, host, r.id.0)
}

// Helper: build a TestServer that mounts only the routing_router with a
// minimal ControllerHandles. The exact ControllerHandles construction
// depends on its actual signature; check `crates/isengard-controller/src/lib.rs`.
async fn test_server() -> (TestServer, Arc<Inventory>, isengard_storage::HostId, i64) {
    let (inv, host, rule_id) = seed_inventory_with_one_rule().await;

    // The dashboard tests share a fixture pattern — look at e.g.
    // dashboard/src/api.rs::tests for how ControllerHandles is built in a
    // test harness. For now, sketch:
    let handles = Arc::new(/* ControllerHandles { inventory: inv.clone(), ... } */);
    let app = isengard_plugin_dashboard::api::routing::router().with_state(handles);
    let server = TestServer::new(app).unwrap();
    (server, inv, host, rule_id)
}

#[tokio::test]
async fn list_rules_returns_seeded_rule() {
    let (server, _inv, _host, rule_id) = test_server().await;
    let resp = server.get("/routing/rules").await;
    resp.assert_status_ok();
    let rules: Vec<serde_json::Value> = resp.json();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["id"], rule_id);
    assert_eq!(rules[0]["public_hostname"], "blog.example.com");
}

#[tokio::test]
async fn delete_rule_removes_it_and_returns_204() {
    let (server, inv, host, rule_id) = test_server().await;
    let resp = server.delete(&format!("/routing/rules/{rule_id}")).await;
    resp.assert_status(StatusCode::NO_CONTENT);
    let remaining = inv.list_routing_rules_for_host(host).await.unwrap();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn delete_unknown_id_returns_404() {
    let (server, _inv, _host, _) = test_server().await;
    let resp = server.delete("/routing/rules/99999").await;
    resp.assert_status_not_found();
}
```

Add to `crates/isengard-plugins/dashboard/Cargo.toml` `[dev-dependencies]` if missing:

```toml
axum-test = "17"  # or whatever version is current and compat with axum 0.8
tempfile = "3"
```

(If the existing dashboard tests use a different HTTP test pattern — e.g. `axum::Router::oneshot` directly — match that instead of pulling `axum-test`.)

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing`
Expected: FAIL — endpoints return 501.

- [ ] **Step 3: Implement list/create/update/delete**

Replace the stubs in `routing.rs`:

```rust
use isengard_storage::{
    InsertRoutingRule, RoutingRule, RoutingRuleId, RoutingRuleSource, RoutingRuleState, TlsMode,
};

#[derive(serde::Deserialize)]
struct CreateRuleBody {
    fleet: String,
    host_id: String,           // ULID string; we parse into HostId
    stack_id: Option<i64>,
    service_name: String,
    container_port: u16,
    public_hostname: String,
    protocol: String,
    adapter: String,
    tls_mode: TlsMode,
    healthcheck_path: Option<String>,
    healthcheck_interval_secs: Option<u32>,
    auth: Option<String>,
    state: Option<RoutingRuleState>,
    source: Option<RoutingRuleSource>,
}

async fn list_rules(State(handles): State<Arc<ControllerHandles>>) -> impl IntoResponse {
    // For v1 we list across all hosts. Real fleet-scoping comes when the
    // active-fleet query param lands.
    // Use `Inventory::list_routing_rules_for_host` per host — for now,
    // scan all hosts. (Plan A's `list_routing_rules_for_host` is the only
    // existing query; if a `list_all` doesn't exist, add a simple wrapper
    // here that iterates hosts then concatenates.)
    let hosts = handles.inventory.list_hosts().await.unwrap_or_default();
    let mut out: Vec<RoutingRule> = Vec::new();
    for h in hosts {
        if let Ok(mut rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            out.append(&mut rules);
        }
    }
    Json(out).into_response()
}

async fn create_rule(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateRuleBody>,
) -> impl IntoResponse {
    let host_id = match body.host_id.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id (expected ULID)").into_response(),
    };
    let stack_id = body.stack_id.map(isengard_storage::StackId);
    let insert = InsertRoutingRule {
        fleet: body.fleet,
        host_id,
        stack_id,
        service_name: body.service_name,
        container_port: body.container_port,
        public_hostname: body.public_hostname,
        protocol: body.protocol,
        adapter: body.adapter,
        tls_mode: body.tls_mode,
        healthcheck_path: body.healthcheck_path,
        healthcheck_interval_secs: body.healthcheck_interval_secs.unwrap_or(10),
        auth: body.auth,
        state: body.state.unwrap_or(RoutingRuleState::Pending),
        source: body.source.unwrap_or(RoutingRuleSource::Ui),
        source_container_id: None,
        source_imported_from: None,
    };
    match handles.inventory.insert_routing_rule(insert).await {
        Ok(rule) => (StatusCode::CREATED, Json(rule)).into_response(),
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            (StatusCode::CONFLICT, format!("hostname conflict: {e}")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("insert failed: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct UpdateRuleBody {
    state: Option<RoutingRuleState>,
    container_port: Option<u16>,
    healthcheck_path: Option<Option<String>>, // double Option: Some(None) clears, None leaves alone
    healthcheck_interval_secs: Option<u32>,
    adapter: Option<String>,
    tls_mode: Option<TlsMode>,
    auth: Option<Option<String>>,
}

async fn update_rule(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpdateRuleBody>,
) -> impl IntoResponse {
    // For v1 we only support state updates (the other fields would require
    // a full UPDATE with optional column writes). Plan B's RoutingPusher
    // already has `update_routing_rule_state`; broaden in a later plan.
    if let Some(state) = body.state {
        if let Err(e) = handles.inventory.update_routing_rule_state(RoutingRuleId(id), state).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("update failed: {e}")).into_response();
        }
    }
    // Re-fetch and return.
    // (Inventory doesn't have get_routing_rule(id) yet — list across hosts and find.)
    let hosts = handles.inventory.list_hosts().await.unwrap_or_default();
    for h in hosts {
        if let Ok(rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            if let Some(r) = rules.into_iter().find(|r| r.id.0 == id) {
                return Json(r).into_response();
            }
        }
    }
    (StatusCode::NOT_FOUND, "rule not found").into_response()
}

async fn delete_rule(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    // Confirm exists by listing then matching, otherwise 404.
    let hosts = handles.inventory.list_hosts().await.unwrap_or_default();
    let mut found = false;
    for h in hosts {
        if let Ok(rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            if rules.iter().any(|r| r.id.0 == id) {
                found = true;
                break;
            }
        }
    }
    if !found {
        return (StatusCode::NOT_FOUND, "rule not found").into_response();
    }
    match handles.inventory.delete_routing_rule(RoutingRuleId(id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("delete failed: {e}")).into_response(),
    }
}
```

If `Inventory::list_hosts` doesn't exist, add a small helper either in this task or in a separate storage task. Check what's available; if there's only a per-fleet list, use that.

- [ ] **Step 4: Verify tests pass**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing`
Expected: 3 tests passing (`list_rules_returns_seeded_rule`, `delete_rule_removes_it_and_returns_204`, `delete_unknown_id_returns_404`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/
git commit -m "feat(dashboard): routing rules CRUD endpoints (list/create/update/delete)"
```

---

### Task 3: Implement `routing/rules/:id/overrides` GET + PUT

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/api/routing.rs`
- Modify: `crates/isengard-plugins/dashboard/tests/api_routing.rs` (add 2 tests)

- [ ] **Step 1: Add the failing tests**

Append to `crates/isengard-plugins/dashboard/tests/api_routing.rs`:

```rust
#[tokio::test]
async fn list_overrides_returns_empty_for_new_rule() {
    let (server, _inv, _host, rule_id) = test_server().await;
    let resp = server.get(&format!("/routing/rules/{rule_id}/overrides")).await;
    resp.assert_status_ok();
    let overrides: Vec<serde_json::Value> = resp.json();
    assert!(overrides.is_empty());
}

#[tokio::test]
async fn upsert_override_then_list_returns_value() {
    let (server, _inv, _host, rule_id) = test_server().await;
    let body = serde_json::json!({"value_json": "manual"});
    let resp = server
        .put(&format!("/routing/rules/{rule_id}/overrides/tls_mode"))
        .json(&body)
        .await;
    resp.assert_status_ok();

    let listed = server.get(&format!("/routing/rules/{rule_id}/overrides")).await;
    let overrides: Vec<serde_json::Value> = listed.json();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0]["field"], "tls_mode");
    assert_eq!(overrides[0]["value_json"], "manual");
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing -- list_overrides upsert_override`
Expected: FAIL.

- [ ] **Step 3: Implement**

Replace the override stubs:

```rust
async fn list_overrides(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    match handles.inventory.list_routing_rule_overrides(RoutingRuleId(id)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("list overrides: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct UpsertOverrideBody {
    value_json: serde_json::Value,
}

async fn upsert_override(
    Path((id, field)): Path<(i64, String)>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpsertOverrideBody>,
) -> impl IntoResponse {
    match handles.inventory.upsert_routing_rule_override(RoutingRuleId(id), &field, body.value_json.clone()).await {
        Ok(()) => Json(serde_json::json!({
            "routing_rule_id": id, "field": field, "value_json": body.value_json
        })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("upsert override: {e}")).into_response(),
    }
}
```

- [ ] **Step 4: Tests pass + commit**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing`
Expected: 5 tests now passing.

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/
git commit -m "feat(dashboard): routing rule overrides GET/PUT endpoints"
```

---

### Task 4: Implement `networking/adapter-config/:host_id/:adapter` GET + PUT

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/api/routing.rs`
- Modify: `crates/isengard-plugins/dashboard/tests/api_routing.rs`

- [ ] **Step 1: Add failing tests**

```rust
#[tokio::test]
async fn get_adapter_config_returns_404_when_not_set() {
    let (server, _inv, host, _rule_id) = test_server().await;
    let resp = server.get(&format!("/networking/adapter-config/{}/cf-tunnel", host)).await;
    resp.assert_status_not_found();
}

#[tokio::test]
async fn upsert_then_get_adapter_config_returns_blob() {
    let (server, _inv, host, _rule_id) = test_server().await;
    let body = serde_json::json!({
        "config_json": { "api_token": "secret", "account_id": "acct-1", "zone_id": "zone-1" },
        "enabled": true,
    });
    let resp = server
        .put(&format!("/networking/adapter-config/{}/cf-tunnel", host))
        .json(&body)
        .await;
    resp.assert_status_ok();

    let got = server.get(&format!("/networking/adapter-config/{}/cf-tunnel", host)).await;
    got.assert_status_ok();
    let v: serde_json::Value = got.json();
    assert_eq!(v["adapter"], "cf-tunnel");
    assert!(v["enabled"].as_bool().unwrap());
    assert_eq!(v["config_json"]["api_token"], "secret");
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing -- adapter_config`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
use isengard_storage::UpsertAdapterConfig;

async fn get_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    match handles.inventory.get_adapter_config(host_id, &adapter).await {
        Ok(Some(cfg)) => Json(cfg).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no adapter config for host+adapter").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("get adapter_config: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct UpsertAdapterBody {
    config_json: serde_json::Value,
    enabled: bool,
}

async fn upsert_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpsertAdapterBody>,
) -> impl IntoResponse {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    let ins = UpsertAdapterConfig {
        host_id,
        adapter: adapter.clone(),
        config_json: body.config_json.clone(),
        enabled: body.enabled,
    };
    if let Err(e) = handles.inventory.upsert_adapter_config(ins).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("upsert adapter_config: {e}")).into_response();
    }
    // Read back and return.
    match handles.inventory.get_adapter_config(host_id, &adapter).await {
        Ok(Some(cfg)) => Json(cfg).into_response(),
        Ok(None) => (StatusCode::INTERNAL_SERVER_ERROR, "upsert succeeded but get returned None").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("get after upsert: {e}")).into_response(),
    }
}
```

- [ ] **Step 4: Tests pass + commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/
git commit -m "feat(dashboard): adapter-config GET/PUT endpoints"
```

---

### Task 5: Implement `networking/adapter-config/:host_id/:adapter/test` POST

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/api/routing.rs`
- Modify: `crates/isengard-plugins/dashboard/tests/api_routing.rs`

- [ ] **Step 1: Add failing tests**

```rust
#[tokio::test]
async fn test_adapter_config_for_none_returns_ok_true() {
    let (server, _inv, host, _) = test_server().await;
    // Seed a 'none' adapter config.
    server
        .put(&format!("/networking/adapter-config/{}/none", host))
        .json(&serde_json::json!({ "config_json": {}, "enabled": true }))
        .await;
    let resp = server
        .post(&format!("/networking/adapter-config/{}/none/test", host))
        .await;
    resp.assert_status_ok();
    let v: serde_json::Value = resp.json();
    assert_eq!(v["ok"], true);
}

#[tokio::test]
async fn test_adapter_config_for_unknown_returns_404() {
    let (server, _inv, host, _) = test_server().await;
    let resp = server
        .post(&format!("/networking/adapter-config/{}/cf-tunnel/test", host))
        .await;
    resp.assert_status_not_found();
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-plugin-dashboard --test api_routing -- test_adapter`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
async fn test_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    let cfg = match handles.inventory.get_adapter_config(host_id, &adapter).await {
        Ok(Some(c)) => c,
        Ok(None) => return (StatusCode::NOT_FOUND, "no adapter config to test").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("read adapter_config: {e}")).into_response(),
    };

    let result = match adapter.as_str() {
        "none" => serde_json::json!({ "ok": true, "detail": null }),
        "cf-tunnel" => {
            // Make a single CF API call to validate the token. We don't need a
            // separate adapter-side hook for v1; the dashboard plugin can do
            // the test inline against api.cloudflare.com.
            let token = cfg.config_json.get("api_token").and_then(|v| v.as_str()).unwrap_or("");
            let account_id = cfg.config_json.get("account_id").and_then(|v| v.as_str()).unwrap_or("");
            if token.is_empty() || account_id.is_empty() {
                serde_json::json!({ "ok": false, "error": "missing api_token or account_id" })
            } else {
                let url = format!("https://api.cloudflare.com/client/v4/accounts/{}/cfd_tunnel?per_page=1", account_id);
                match reqwest::Client::new().get(&url).bearer_auth(token).send().await {
                    Ok(resp) if resp.status().is_success() => serde_json::json!({ "ok": true, "detail": null }),
                    Ok(resp) => serde_json::json!({ "ok": false, "error": format!("CF API returned {}", resp.status()) }),
                    Err(e) => serde_json::json!({ "ok": false, "error": format!("CF API request failed: {e}") }),
                }
            }
        }
        "tailscale" => {
            // Tailscale test would require running `tailscale status --json` on
            // an agent host. The controller doesn't have a CLI to invoke.
            // For v1, return `ok: true` if config exists; document that real
            // status comes from the agent's heartbeat.
            serde_json::json!({ "ok": true, "detail": "tailscale runtime status surfaces via agent heartbeat" })
        }
        other => serde_json::json!({ "ok": false, "error": format!("unknown adapter: {other}") }),
    };

    Json(result).into_response()
}
```

Add `reqwest = { workspace = true, features = ["rustls-tls", "json"] }` to dashboard's `[dependencies]` if not already present (it likely is — check first).

- [ ] **Step 4: Tests pass + commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/
git commit -m "feat(dashboard): adapter-config test-connection endpoint"
```

---

## Phase 8h-2: Convert /settings to tabs

### Task 6: SettingsTabs.vue + refactor /settings/index.vue

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`

- [ ] **Step 1: Create the tab container**

Create `crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue`:

```vue
<template>
  <div>
    <nav class="flex gap-1 border-b border-iso-border mb-6">
      <button
        v-for="tab in tabs"
        :key="tab.key"
        :class="[
          'px-4 py-2 text-sm font-medium border-b-2 -mb-px',
          activeTab === tab.key
            ? 'border-iso-info text-iso-text-primary'
            : 'border-transparent text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border',
        ]"
        @click="setActive(tab.key)"
      >
        {{ tab.label }}
      </button>
    </nav>

    <div>
      <slot :active-tab="activeTab" />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'

interface Tab {
  key: string
  label: string
}

const props = defineProps<{ tabs: Tab[]; defaultTab?: string }>()
const route = useRoute()
const router = useRouter()

const activeTab = computed(() => {
  const t = route.query.tab as string | undefined
  if (t && props.tabs.some(tab => tab.key === t)) return t
  return props.defaultTab ?? props.tabs[0]?.key ?? ''
})

function setActive(key: string) {
  router.push({ query: { ...route.query, tab: key } })
}
</script>
```

- [ ] **Step 2: Refactor /settings/index.vue**

Replace `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`:

```vue
<template>
  <AppShell>
    <PageHeader title="Settings" subtitle="Controller configuration" />
    <div class="flex-1 overflow-y-auto">
      <div class="max-w-3xl mx-auto w-full p-6">
        <SettingsTabs :tabs="tabs" default-tab="general" v-slot="{ activeTab }">
          <div v-if="activeTab === 'general'">
            <FleetsSettings />
            <EnrollmentSettings />
          </div>
          <div v-else-if="activeTab === 'networking'">
            <NetworkingSettings />
          </div>
          <div v-else-if="activeTab === 'notifier'">
            <NotifierSettings />
          </div>
        </SettingsTabs>
      </div>
    </div>
  </AppShell>
</template>

<script setup lang="ts">
const tabs = [
  { key: 'general', label: 'General' },
  { key: 'networking', label: 'Networking' },
  { key: 'notifier', label: 'Notifier' },
]
</script>
```

`NetworkingSettings` doesn't exist yet — Task 7 creates it. The build will fail until Task 7 lands. To keep this commit green, add a temporary stub:

Create `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`:

```vue
<template>
  <div class="p-4 text-iso-text-muted text-sm">
    Networking settings — coming up in PB-T7.
  </div>
</template>
```

- [ ] **Step 3: Verify the dashboard builds**

Run: `cd crates/isengard-plugins/dashboard/web && bun install && bun run build`
Expected: success.

(If `bun install` is needed for the first time in the worktree, allow it. If not, just `bun run build`.)

Then `cargo build -p isengard-plugin-dashboard` — the bundle is embedded via `rust-embed`.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/web/components/settings/SettingsTabs.vue \
        crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue \
        crates/isengard-plugins/dashboard/web/pages/settings/index.vue
git commit -m "feat(dashboard): convert /settings to tabs (general/networking/notifier)"
```

---

## Phase 8h-3: Networking tab body

### Task 7: useRoutingRules composable + RoutingRulesTable + NetworkingSettings shell

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useRoutingRules.ts`
- Create: `crates/isengard-plugins/dashboard/web/components/RoutingRulesTable.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`

- [ ] **Step 1: Composable**

Create `crates/isengard-plugins/dashboard/web/composables/useRoutingRules.ts`:

```ts
import { ref, onMounted } from 'vue'

export interface RoutingRule {
  id: number
  fleet: string
  host_id: string
  service_name: string
  container_port: number
  public_hostname: string
  protocol: string
  adapter: string
  tls_mode: 'edge' | 'acme' | 'manual'
  healthcheck_path: string | null
  healthcheck_interval_secs: number
  state: 'pending' | 'active' | 'draining' | 'failed'
  source: 'ui' | 'label' | 'imported'
  source_container_id: string | null
}

export function useRoutingRules() {
  const rules = ref<RoutingRule[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  const { useApi } = useNuxtApp() as any  // adapt to existing useApi composable shape
  const apiBase = '/api/routing/rules'

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      const resp = await fetch(apiBase)
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`)
      rules.value = await resp.json()
    } catch (e: any) {
      error.value = String(e)
    } finally {
      loading.value = false
    }
  }

  async function createRule(body: Partial<RoutingRule>) {
    const resp = await fetch(apiBase, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!resp.ok) throw new Error(`${resp.status}`)
    await refresh()
  }

  async function updateRule(id: number, body: Partial<RoutingRule>) {
    const resp = await fetch(`${apiBase}/${id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!resp.ok) throw new Error(`${resp.status}`)
    await refresh()
  }

  async function deleteRule(id: number) {
    const resp = await fetch(`${apiBase}/${id}`, { method: 'DELETE' })
    if (!resp.ok) throw new Error(`${resp.status}`)
    await refresh()
  }

  onMounted(refresh)

  return { rules, loading, error, refresh, createRule, updateRule, deleteRule }
}
```

(The `useApi` shape may differ — check `composables/useApi.ts` and use that pattern instead of raw `fetch`. The above is a minimal stand-alone version that works regardless.)

- [ ] **Step 2: RoutingRulesTable component**

Create `crates/isengard-plugins/dashboard/web/components/RoutingRulesTable.vue`:

```vue
<template>
  <div>
    <div class="flex items-center justify-between mb-4">
      <h3 class="text-sm font-semibold text-iso-text-primary">Routing rules</h3>
      <button
        class="px-3 py-1.5 text-xs font-medium bg-iso-info text-iso-bg-base rounded"
        @click="emit('add')"
      >
        + Add rule
      </button>
    </div>

    <div v-if="loading" class="text-iso-text-muted text-sm">Loading…</div>
    <div v-else-if="error" class="text-iso-error text-sm">Error: {{ error }}</div>
    <div v-else-if="rules.length === 0" class="text-iso-text-muted text-sm">
      No routing rules. Add a rule above, or apply <code>isengard.expose</code> labels to your containers.
    </div>
    <table v-else class="w-full text-xs">
      <thead class="text-iso-text-muted">
        <tr>
          <th class="text-left pb-2">Hostname</th>
          <th class="text-left pb-2">Target</th>
          <th class="text-left pb-2">Adapter</th>
          <th class="text-left pb-2">TLS</th>
          <th class="text-left pb-2">State</th>
          <th class="text-left pb-2">Source</th>
          <th class="pb-2"></th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="r in rules" :key="r.id" class="border-t border-iso-border">
          <td class="py-2 mono">{{ r.public_hostname }}</td>
          <td class="py-2 mono">{{ r.service_name }}:{{ r.container_port }}</td>
          <td class="py-2">{{ r.adapter }}</td>
          <td class="py-2">{{ r.tls_mode }}</td>
          <td class="py-2">{{ r.state }}</td>
          <td class="py-2">
            <span v-if="r.source === 'label'">🏷️ label · <code>{{ r.source_container_id?.slice(0, 8) }}</code></span>
            <span v-else-if="r.source === 'imported'">📥 imported</span>
            <span v-else>🎨 ui</span>
          </td>
          <td class="py-2 text-right">
            <button class="text-iso-text-muted hover:text-iso-text-primary px-2" @click="emit('edit', r)">
              Edit
            </button>
            <button class="text-iso-error hover:underline px-2" @click="onDelete(r)">
              Delete
            </button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<script setup lang="ts">
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'

const { rules, loading, error, deleteRule } = useRoutingRules()
const emit = defineEmits<{ (e: 'add'): void; (e: 'edit', rule: RoutingRule): void }>()

async function onDelete(r: RoutingRule) {
  if (!confirm(`Delete rule for ${r.public_hostname}?`)) return
  await deleteRule(r.id)
}
</script>
```

- [ ] **Step 3: Replace NetworkingSettings stub**

Replace `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`:

```vue
<template>
  <div class="space-y-8">
    <section>
      <h2 class="text-sm font-semibold text-iso-text-primary mb-4">Adapters</h2>
      <div class="text-iso-text-muted text-sm">Adapter cards land in PB-T9.</div>
    </section>
    <section>
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
    </section>
  </div>
</template>

<script setup lang="ts">
import type { RoutingRule } from '~/composables/useRoutingRules'

function onAdd() {
  // Modal hook lands in PB-T8.
  alert('Add rule modal: PB-T8')
}
function onEdit(r: RoutingRule) {
  alert(`Edit rule modal: PB-T8 (id=${r.id})`)
}
</script>
```

- [ ] **Step 4: Build verify**

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

Run: `cargo build -p isengard-plugin-dashboard`
Expected: success.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/web/composables/useRoutingRules.ts \
        crates/isengard-plugins/dashboard/web/components/RoutingRulesTable.vue \
        crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue
git commit -m "feat(dashboard): networking tab shell + routing rules table + composable"
```

---

## Phase 8h-4: Modals

### Task 8: RoutingRuleEditModal + ServiceExposeModal + wire from NetworkingSettings + Stack detail

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/RoutingRuleEditModal.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/ServiceExposeModal.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`

- [ ] **Step 1: RoutingRuleEditModal**

Create `crates/isengard-plugins/dashboard/web/components/RoutingRuleEditModal.vue`:

```vue
<template>
  <Dialog :open="open" @update:open="emit('update:open', $event)">
    <DialogContent class="max-w-md">
      <DialogHeader>
        <DialogTitle>{{ rule ? 'Edit routing rule' : 'Add routing rule' }}</DialogTitle>
      </DialogHeader>

      <div class="space-y-4 py-4">
        <div>
          <label class="text-xs text-iso-text-muted">Hostname</label>
          <input v-model="form.public_hostname" type="text" placeholder="blog.example.com"
                 class="w-full mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
        </div>

        <div class="grid grid-cols-2 gap-3">
          <div>
            <label class="text-xs text-iso-text-muted">Service</label>
            <input v-model="form.service_name" type="text" placeholder="web"
                   class="w-full mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
          </div>
          <div>
            <label class="text-xs text-iso-text-muted">Container port</label>
            <input v-model.number="form.container_port" type="number"
                   class="w-full mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">Adapter</label>
          <div class="flex gap-2 mt-1">
            <label v-for="a in ['none', 'tailscale', 'cf-tunnel']" :key="a"
                   class="flex items-center gap-1 text-xs cursor-pointer">
              <input v-model="form.adapter" type="radio" :value="a" />
              {{ a }}
            </label>
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">TLS mode</label>
          <div class="flex gap-2 mt-1">
            <label v-for="m in ['edge', 'acme', 'manual']" :key="m"
                   class="flex items-center gap-1 text-xs cursor-pointer">
              <input v-model="form.tls_mode" type="radio" :value="m" />
              {{ m }}
            </label>
          </div>
        </div>

        <div>
          <label class="text-xs text-iso-text-muted">Healthcheck path (optional)</label>
          <input v-model="form.healthcheck_path" type="text" placeholder="/healthz"
                 class="w-full mono text-sm border border-iso-border rounded px-2 py-1.5 bg-iso-bg-elevated" />
        </div>
      </div>

      <DialogFooter>
        <button class="px-3 py-1.5 text-sm text-iso-text-muted" @click="emit('update:open', false)">
          Cancel
        </button>
        <button class="px-3 py-1.5 text-sm bg-iso-info text-iso-bg-base rounded font-medium"
                :disabled="!canSave" @click="onSave">
          {{ rule ? 'Save' : 'Create' }}
        </button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>

<script setup lang="ts">
import { reactive, computed, watch } from 'vue'
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '~/components/ui/dialog'
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'

const props = defineProps<{ open: boolean; rule?: RoutingRule | null; defaultHostId?: string }>()
const emit = defineEmits<{ (e: 'update:open', v: boolean): void }>()

const { createRule, updateRule } = useRoutingRules()

const form = reactive<Partial<RoutingRule>>({
  public_hostname: '',
  service_name: '',
  container_port: 8080,
  adapter: 'none',
  tls_mode: 'acme',
  healthcheck_path: null,
  fleet: 'default',
  source: 'ui',
})

watch(() => props.rule, (r) => {
  if (r) {
    Object.assign(form, r)
  } else {
    Object.assign(form, {
      public_hostname: '', service_name: '', container_port: 8080,
      adapter: 'none', tls_mode: 'acme', healthcheck_path: null,
      fleet: 'default', source: 'ui',
    })
  }
}, { immediate: true })

const canSave = computed(() => !!form.public_hostname && !!form.service_name && !!form.container_port)

async function onSave() {
  const body: any = { ...form }
  if (props.defaultHostId && !body.host_id) body.host_id = props.defaultHostId
  if (props.rule) {
    await updateRule(props.rule.id, body)
  } else {
    await createRule(body)
  }
  emit('update:open', false)
}
</script>
```

- [ ] **Step 2: ServiceExposeModal (thin wrapper around RoutingRuleEditModal)**

Create `crates/isengard-plugins/dashboard/web/components/ServiceExposeModal.vue`:

```vue
<template>
  <RoutingRuleEditModal
    :open="open"
    :rule="seedRule"
    :default-host-id="hostId"
    @update:open="emit('update:open', $event)"
  />
</template>

<script setup lang="ts">
import { computed } from 'vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

const props = defineProps<{
  open: boolean
  hostId: string
  serviceName: string
  containerPort: number
}>()
const emit = defineEmits<{ (e: 'update:open', v: boolean): void }>()

const seedRule = computed<Partial<RoutingRule>>(() => ({
  service_name: props.serviceName,
  container_port: props.containerPort,
  source: 'ui',
}))
</script>
```

- [ ] **Step 3: Wire into NetworkingSettings**

Replace `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`:

```vue
<template>
  <div class="space-y-8">
    <section>
      <h2 class="text-sm font-semibold text-iso-text-primary mb-4">Adapters</h2>
      <div class="text-iso-text-muted text-sm">Adapter cards land in PB-T9.</div>
    </section>
    <section>
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
    </section>

    <RoutingRuleEditModal v-model:open="modalOpen" :rule="editingRule" />
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import RoutingRulesTable from '~/components/RoutingRulesTable.vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

const modalOpen = ref(false)
const editingRule = ref<RoutingRule | null>(null)

function onAdd() {
  editingRule.value = null
  modalOpen.value = true
}
function onEdit(r: RoutingRule) {
  editingRule.value = r
  modalOpen.value = true
}
</script>
```

- [ ] **Step 4: Wire ServiceExposeModal into Stack detail**

Find the existing "Expose" button in `crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue`. Add the modal:

```vue
<!-- in template, near the existing Expose button: -->
<ServiceExposeModal
  v-if="exposeModalOpen"
  v-model:open="exposeModalOpen"
  :host-id="exposeModalHostId"
  :service-name="exposeModalServiceName"
  :container-port="exposeModalPort"
/>

<!-- and in the existing Expose button click handler: -->
<button @click="openExposeFor(host.id, svc.name, svc.first_port)">Expose</button>

<!-- script: -->
const exposeModalOpen = ref(false)
const exposeModalHostId = ref('')
const exposeModalServiceName = ref('')
const exposeModalPort = ref(0)
function openExposeFor(hostId: string, serviceName: string, port: number) {
  exposeModalHostId.value = hostId
  exposeModalServiceName.value = serviceName
  exposeModalPort.value = port
  exposeModalOpen.value = true
}
```

If the existing `[id].vue` doesn't already have an Expose button, just import the component and a placeholder button — Plan A's mocks anticipated it but the page may not have shipped it yet.

- [ ] **Step 5: Build verify + commit**

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/web/components/RoutingRuleEditModal.vue \
        crates/isengard-plugins/dashboard/web/components/ServiceExposeModal.vue \
        crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue
git commit -m "feat(dashboard): RoutingRuleEditModal + ServiceExposeModal wired into networking + stack detail"
```

---

## Phase 8h-5: Adapter cards

### Task 9: useAdapterConfig composable + 3 adapter cards + wire into NetworkingSettings

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useAdapterConfig.ts`
- Create: `crates/isengard-plugins/dashboard/web/components/AdapterCardNone.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/AdapterCardTailscale.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/AdapterCardCfTunnel.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue`

- [ ] **Step 1: Composable**

Create `crates/isengard-plugins/dashboard/web/composables/useAdapterConfig.ts`:

```ts
import { ref } from 'vue'

export interface AdapterConfig {
  host_id: string
  adapter: string
  config_json: Record<string, any>
  enabled: boolean
}

export interface TestResult {
  ok: boolean
  error?: string
  detail?: any
}

export function useAdapterConfig(hostId: string, adapter: string) {
  const config = ref<AdapterConfig | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)
  const testResult = ref<TestResult | null>(null)

  const apiBase = `/api/networking/adapter-config/${hostId}/${adapter}`

  async function load() {
    loading.value = true
    error.value = null
    try {
      const resp = await fetch(apiBase)
      if (resp.status === 404) { config.value = null; return }
      if (!resp.ok) throw new Error(`${resp.status}`)
      config.value = await resp.json()
    } catch (e: any) {
      error.value = String(e)
    } finally {
      loading.value = false
    }
  }

  async function save(config_json: Record<string, any>, enabled: boolean) {
    const resp = await fetch(apiBase, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ config_json, enabled }),
    })
    if (!resp.ok) throw new Error(`${resp.status}`)
    config.value = await resp.json()
  }

  async function test() {
    const resp = await fetch(`${apiBase}/test`, { method: 'POST' })
    if (!resp.ok) {
      testResult.value = { ok: false, error: `${resp.status}` }
      return
    }
    testResult.value = await resp.json()
  }

  return { config, loading, error, testResult, load, save, test }
}
```

- [ ] **Step 2: AdapterCardNone (minimal)**

Create `crates/isengard-plugins/dashboard/web/components/AdapterCardNone.vue`:

```vue
<template>
  <div class="border border-iso-border rounded-lg p-4 bg-iso-bg-elevated">
    <div class="flex items-center justify-between mb-3">
      <h3 class="text-sm font-semibold">none</h3>
      <span class="text-[10px] uppercase px-2 py-0.5 rounded bg-iso-bg-base text-iso-text-muted">baseline</span>
    </div>
    <p class="text-xs text-iso-text-muted">
      No external adapter. Routing rules with adapter=none rely on the host having a public IP +
      DNS pointed at it. Pingora handles TLS via Let's Encrypt (HTTP-01).
    </p>
  </div>
</template>
```

- [ ] **Step 3: AdapterCardTailscale**

Create `crates/isengard-plugins/dashboard/web/components/AdapterCardTailscale.vue`:

```vue
<template>
  <div class="border border-iso-border rounded-lg p-4 bg-iso-bg-elevated space-y-3">
    <div class="flex items-center justify-between">
      <h3 class="text-sm font-semibold">tailscale</h3>
      <span :class="statusClass">{{ statusLabel }}</span>
    </div>

    <p class="text-xs text-iso-text-muted">
      Drives the user's <code>tailscale</code> CLI. Cert provided by tailnet. Toggle Funnel to
      make the hostname public.
    </p>

    <div class="space-y-2">
      <label class="flex items-center gap-2 text-xs">
        <input v-model="enabled" type="checkbox" />
        Enabled
      </label>
      <label class="flex items-center gap-2 text-xs">
        <input v-model="funnel" type="checkbox" />
        Funnel (public exposure)
      </label>
    </div>

    <div class="flex gap-2">
      <button class="px-3 py-1 text-xs bg-iso-info text-iso-bg-base rounded" @click="onSave">
        Save
      </button>
      <button class="px-3 py-1 text-xs border border-iso-border rounded" @click="onTest">
        Test connection
      </button>
    </div>

    <div v-if="testResult" class="text-xs">
      <span v-if="testResult.ok" class="text-iso-success">✓ {{ testResult.detail || 'ok' }}</span>
      <span v-else class="text-iso-error">✗ {{ testResult.error }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, onMounted, computed } from 'vue'
import { useAdapterConfig } from '~/composables/useAdapterConfig'

const props = defineProps<{ hostId: string }>()
const { config, testResult, load, save, test } = useAdapterConfig(props.hostId, 'tailscale')

const enabled = ref(false)
const funnel = ref(false)

const statusLabel = computed(() => {
  if (!config.value) return 'not configured'
  if (testResult.value?.ok === false) return 'error'
  if (config.value.enabled) return 'configured'
  return 'disabled'
})
const statusClass = computed(() => {
  const base = 'text-[10px] uppercase px-2 py-0.5 rounded'
  if (statusLabel.value === 'configured') return `${base} bg-iso-success/20 text-iso-success`
  if (statusLabel.value === 'error') return `${base} bg-iso-error/20 text-iso-error`
  if (statusLabel.value === 'disabled') return `${base} bg-iso-warn/20 text-iso-warn`
  return `${base} bg-iso-bg-base text-iso-text-muted`
})

watch(config, (c) => {
  if (c) {
    enabled.value = c.enabled
    funnel.value = !!c.config_json?.funnel
  }
})

async function onSave() {
  await save({ funnel: funnel.value }, enabled.value)
}
async function onTest() {
  await test()
}

onMounted(load)
</script>
```

- [ ] **Step 4: AdapterCardCfTunnel**

Create `crates/isengard-plugins/dashboard/web/components/AdapterCardCfTunnel.vue`:

```vue
<template>
  <div class="border border-iso-border rounded-lg p-4 bg-iso-bg-elevated space-y-3">
    <div class="flex items-center justify-between">
      <h3 class="text-sm font-semibold">cf-tunnel</h3>
      <span :class="statusClass">{{ statusLabel }}</span>
    </div>

    <p class="text-xs text-iso-text-muted">
      Cloudflare Tunnel. Spawns supervised <code>cloudflared</code> + manages tunnel/DNS via the
      CF v4 API. Tokens stored on the controller; protect controller access accordingly.
    </p>

    <div class="space-y-2 text-xs">
      <label class="block">
        <span class="text-iso-text-muted">API token</span>
        <div class="flex gap-1">
          <input v-model="form.api_token" :type="showToken ? 'text' : 'password'"
                 class="flex-1 mono border border-iso-border rounded px-2 py-1 bg-iso-bg-base" />
          <button class="px-2 text-iso-text-muted text-[10px]" @click="showToken = !showToken">
            {{ showToken ? 'Hide' : 'Show' }}
          </button>
        </div>
      </label>

      <label class="block">
        <span class="text-iso-text-muted">Account ID</span>
        <input v-model="form.account_id" type="text"
               class="w-full mono border border-iso-border rounded px-2 py-1 bg-iso-bg-base" />
      </label>

      <label class="block">
        <span class="text-iso-text-muted">Zone ID</span>
        <input v-model="form.zone_id" type="text"
               class="w-full mono border border-iso-border rounded px-2 py-1 bg-iso-bg-base" />
      </label>

      <label class="block">
        <span class="text-iso-text-muted">Tunnel name</span>
        <input v-model="form.tunnel_name" type="text" placeholder="isengard"
               class="w-full mono border border-iso-border rounded px-2 py-1 bg-iso-bg-base" />
      </label>

      <label class="block" v-if="form.tunnel_id">
        <span class="text-iso-text-muted">Tunnel ID (read-only)</span>
        <input :value="form.tunnel_id" type="text" disabled
               class="w-full mono border border-iso-border rounded px-2 py-1 bg-iso-bg-base text-iso-text-faint" />
      </label>

      <label class="flex items-center gap-2">
        <input v-model="enabled" type="checkbox" />
        Enabled
      </label>
    </div>

    <div class="flex gap-2">
      <button class="px-3 py-1 text-xs bg-iso-info text-iso-bg-base rounded" @click="onSave">
        Save
      </button>
      <button class="px-3 py-1 text-xs border border-iso-border rounded" @click="onTest">
        Test connection
      </button>
    </div>

    <div v-if="testResult" class="text-xs">
      <span v-if="testResult.ok" class="text-iso-success">✓ token works</span>
      <span v-else class="text-iso-error">✗ {{ testResult.error }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { reactive, ref, watch, onMounted, computed } from 'vue'
import { useAdapterConfig } from '~/composables/useAdapterConfig'

const props = defineProps<{ hostId: string }>()
const { config, testResult, load, save, test } = useAdapterConfig(props.hostId, 'cf-tunnel')

const form = reactive({
  api_token: '',
  account_id: '',
  zone_id: '',
  tunnel_name: 'isengard',
  tunnel_id: '',
  tunnel_token: '',
})
const enabled = ref(false)
const showToken = ref(false)

const statusLabel = computed(() => {
  if (!config.value) return 'not configured'
  if (testResult.value?.ok === false) return 'error'
  if (config.value.enabled) return 'configured'
  return 'disabled'
})
const statusClass = computed(() => {
  const base = 'text-[10px] uppercase px-2 py-0.5 rounded'
  if (statusLabel.value === 'configured') return `${base} bg-iso-success/20 text-iso-success`
  if (statusLabel.value === 'error') return `${base} bg-iso-error/20 text-iso-error`
  if (statusLabel.value === 'disabled') return `${base} bg-iso-warn/20 text-iso-warn`
  return `${base} bg-iso-bg-base text-iso-text-muted`
})

watch(config, (c) => {
  if (c) {
    Object.assign(form, c.config_json)
    enabled.value = c.enabled
  }
})

async function onSave() {
  await save({ ...form }, enabled.value)
}
async function onTest() {
  await test()
}

onMounted(load)
</script>
```

- [ ] **Step 5: Wire adapter cards into NetworkingSettings**

Replace the `<section>` with adapter cards. Final `NetworkingSettings.vue`:

```vue
<template>
  <div class="space-y-8">
    <section>
      <h2 class="text-sm font-semibold text-iso-text-primary mb-4">Adapters</h2>
      <div class="text-iso-text-muted text-xs mb-3" v-if="!firstHost">
        No hosts enrolled yet. Add a host first, then configure adapters per-host.
      </div>
      <div v-else class="grid gap-4">
        <AdapterCardNone />
        <AdapterCardTailscale :host-id="firstHost.id" />
        <AdapterCardCfTunnel :host-id="firstHost.id" />
      </div>
    </section>
    <section>
      <RoutingRulesTable @add="onAdd" @edit="onEdit" />
    </section>

    <RoutingRuleEditModal v-model:open="modalOpen" :rule="editingRule" />
  </div>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useHostsStore } from '~/stores/hosts'  // existing pinia store
import RoutingRulesTable from '~/components/RoutingRulesTable.vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import AdapterCardNone from '~/components/AdapterCardNone.vue'
import AdapterCardTailscale from '~/components/AdapterCardTailscale.vue'
import AdapterCardCfTunnel from '~/components/AdapterCardCfTunnel.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

const hostsStore = useHostsStore()
const firstHost = computed(() => hostsStore.hosts[0])

const modalOpen = ref(false)
const editingRule = ref<RoutingRule | null>(null)
function onAdd() { editingRule.value = null; modalOpen.value = true }
function onEdit(r: RoutingRule) { editingRule.value = r; modalOpen.value = true }
</script>
```

(For v1 we show adapter cards for the first host only. Per-host card layout is a v1.x follow-up. The exact pinia store path may differ — adapt to `~/stores/hosts` or wherever hosts state lives.)

- [ ] **Step 6: Build verify + commit**

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

Run: `cargo build -p isengard-plugin-dashboard`
Expected: success.

```bash
cargo fmt --all
git add crates/isengard-plugins/dashboard/web/composables/useAdapterConfig.ts \
        crates/isengard-plugins/dashboard/web/components/AdapterCard*.vue \
        crates/isengard-plugins/dashboard/web/components/NetworkingSettings.vue
git commit -m "feat(dashboard): adapter cards (none/tailscale/cf-tunnel) + useAdapterConfig"
```

---

## Phase 8i-1: Upstream state field

### Task 10: Add `Upstream.state: UpstreamState` + `UpstreamRegistry::set_state` / `remove_if_draining`

**Files:**
- Modify: `crates/isengard-agent/src/proxy/upstreams.rs`

- [ ] **Step 1: Read existing Upstream struct**

Read `crates/isengard-agent/src/proxy/upstreams.rs` to confirm current shape. From Plan A it has `container_id`, `addr`, `healthy`, `health_path`, `health_interval`, `consecutive_failures`.

- [ ] **Step 2: Write failing test**

Append to `crates/isengard-agent/src/proxy/upstreams.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn sample(host: &str) -> Upstream {
        Upstream {
            container_id: "c".into(),
            addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            healthy: true,
            health_path: None,
            health_interval: Duration::from_secs(10),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        }
    }

    #[test]
    fn set_state_changes_state_field() {
        let mut reg = UpstreamRegistry::new();
        reg.set("h.test", sample("h.test"));
        reg.set_state("h.test", UpstreamState::Draining);
        assert_eq!(reg.get("h.test").unwrap().state, UpstreamState::Draining);
    }

    #[test]
    fn remove_if_draining_removes_only_draining_entries() {
        let mut reg = UpstreamRegistry::new();
        reg.set("a.test", sample("a.test"));
        reg.set("b.test", sample("b.test"));
        reg.set_state("b.test", UpstreamState::Draining);

        reg.remove_if_draining("a.test"); // active → no-op
        reg.remove_if_draining("b.test"); // draining → removed
        assert!(reg.get("a.test").is_some(), "active should remain");
        assert!(reg.get("b.test").is_none(), "draining should be removed");
    }

    #[test]
    fn upstream_default_state_is_active() {
        let u = sample("h.test");
        assert_eq!(u.state, UpstreamState::Active);
    }
}
```

- [ ] **Step 3: Run to fail**

Run: `cargo test -p isengard-agent upstreams::tests`
Expected: FAIL — `UpstreamState` undefined.

- [ ] **Step 4: Add `UpstreamState` + extend `Upstream` + add registry methods**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    Active,
    Draining,
}

#[derive(Debug, Clone)]
pub struct Upstream {
    pub container_id: String,
    pub addr: SocketAddr,
    pub healthy: bool,
    pub health_path: Option<String>,
    pub health_interval: Duration,
    pub consecutive_failures: u32,
    /// Lifecycle state. New entries default to Active. Atomic upstream
    /// swap (Phase 8i Plan C) sets this to Draining; the router then
    /// stops sending new connections to it.
    pub state: UpstreamState,
}

// In UpstreamRegistry:

impl UpstreamRegistry {
    // ... existing methods ...

    pub fn set_state(&mut self, hostname: &str, state: UpstreamState) {
        if let Some(u) = self.by_hostname.get_mut(hostname) {
            u.state = state;
        }
    }

    pub fn remove_if_draining(&mut self, hostname: &str) {
        if let Some(u) = self.by_hostname.get(hostname) {
            if u.state == UpstreamState::Draining {
                self.by_hostname.remove(hostname);
            }
        }
    }
}
```

Default `state: UpstreamState::Active` everywhere `Upstream { ... }` is constructed in the codebase. Search for those literals (likely in `apply_config`, the existing tests `proxy_basic_routing.rs`, `proxy_apply_config.rs`, `proxy_eviction.rs`, `proxy_health_event.rs`) and add `state: UpstreamState::Active,` to each.

- [ ] **Step 5: Run all tests**

Run: `cargo test --workspace`
Expected: green (the new unit tests + all pre-existing tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/proxy/upstreams.rs \
        crates/isengard-agent/tests/  # if any test files needed Upstream literal updates
git commit -m "feat(agent): UpstreamState (Active/Draining) + set_state + remove_if_draining"
```

---

## Phase 8i-2: swap_upstream + router guard

### Task 11: `proxy/swap.rs` + router skip-draining guard + 3 unit tests

**Files:**
- Create: `crates/isengard-agent/src/proxy/swap.rs`
- Create: `crates/isengard-agent/tests/proxy_swap_unit.rs`
- Modify: `crates/isengard-agent/src/proxy/mod.rs`
- Modify: `crates/isengard-agent/src/proxy/router.rs`

- [ ] **Step 1: Add the failing tests**

Create `crates/isengard-agent/tests/proxy_swap_unit.rs`:

```rust
use isengard_agent::proxy::{ProxyState, Upstream, UpstreamState, swap_upstream};
use std::net::SocketAddr;
use std::time::Duration;

fn sample(addr_port: u16) -> Upstream {
    Upstream {
        container_id: format!("c-{addr_port}"),
        addr: SocketAddr::from(([127, 0, 0, 1], addr_port)),
        healthy: true,
        health_path: None,
        health_interval: Duration::from_secs(10),
        consecutive_failures: 0,
        state: UpstreamState::Active,
    }
}

#[tokio::test]
async fn swap_marks_old_as_draining_then_installs_new() {
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));

    let new_up = sample(9090);
    swap_upstream(&state, "h.test", new_up.clone(), Duration::from_secs(60)).await.unwrap();

    let r = state.upstreams.read().await;
    let after = r.get("h.test").unwrap();
    assert_eq!(after.addr, new_up.addr, "new upstream is now serving");
    assert_eq!(after.state, UpstreamState::Active, "new entry is active");
}

#[tokio::test]
async fn draining_upstream_removed_after_grace_period() {
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));

    // Mark the existing entry as draining manually (don't install a replacement
    // — we want to test the timer side-effect).
    state.upstreams.write().await.set_state("h.test", UpstreamState::Draining);

    // Use the same drain helper from swap.rs by calling swap with a no-op
    // setup: set_state to draining + spawn the cleanup with a tiny grace.
    // (Calling swap_upstream with the same hostname would replace; we want
    // to test the timer alone. swap.rs exposes `spawn_drain_cleanup` for
    // direct testing.)
    isengard_agent::proxy::swap::spawn_drain_cleanup(state.clone(), "h.test".into(), Duration::from_millis(100));

    tokio::time::sleep(Duration::from_millis(250)).await;

    let r = state.upstreams.read().await;
    assert!(r.get("h.test").is_none(), "draining entry should be removed after grace");
}

#[tokio::test]
async fn cleanup_leaves_active_entry_alone() {
    // If the entry has been replaced with an Active one before the timer
    // fires, the cleanup should NOT remove it.
    let state = ProxyState::new();
    state.upstreams.write().await.set("h.test", sample(8080));
    isengard_agent::proxy::swap::spawn_drain_cleanup(state.clone(), "h.test".into(), Duration::from_millis(50));

    // Before the timer fires, "replace" the entry with an Active one
    // (simulates a second swap).
    state.upstreams.write().await.set("h.test", sample(9090));

    tokio::time::sleep(Duration::from_millis(150)).await;

    let r = state.upstreams.read().await;
    let entry = r.get("h.test").expect("entry should still exist");
    assert_eq!(entry.state, UpstreamState::Active);
    assert_eq!(entry.addr.port(), 9090);
}
```

- [ ] **Step 2: Run to fail**

Run: `cargo test -p isengard-agent --test proxy_swap_unit`
Expected: FAIL — `swap_upstream` and `swap::spawn_drain_cleanup` undefined.

- [ ] **Step 3: Implement `swap.rs`**

Create `crates/isengard-agent/src/proxy/swap.rs`:

```rust
//! Atomic upstream swap with drain semantics. Phase 8i — provides the
//! primitive that Phase 10 (Blue-Green Deployment) calls.

use crate::proxy::{ProxyState, Upstream, UpstreamState};
use anyhow::Result;
use std::time::Duration;

/// Replace the upstream for `hostname` with `new_upstream`, marking the
/// existing entry as `Draining` so the router stops routing new
/// connections to it. After `grace_period`, the draining entry is
/// removed from the registry (existing in-flight connections continue
/// against the upstream they were already routed to).
///
/// If no entry exists for `hostname` yet, this acts as a plain insert.
///
/// Returns immediately after installing the new upstream. The cleanup
/// timer runs as a background tokio task.
pub async fn swap_upstream(
    state: &ProxyState,
    hostname: &str,
    new_upstream: Upstream,
    grace_period: Duration,
) -> Result<()> {
    // 1. Mark current as draining (no new connections will be routed).
    {
        let mut w = state.upstreams.write().await;
        w.set_state(hostname, UpstreamState::Draining);
    }

    // 2. Spawn the drain cleanup timer.
    spawn_drain_cleanup(state.clone(), hostname.to_string(), grace_period);

    // 3. Install the new upstream (overwrites the registry entry).
    let mut new_active = new_upstream;
    new_active.state = UpstreamState::Active;
    state.upstreams.write().await.set(hostname.to_string(), new_active);

    Ok(())
}

/// Spawn a tokio task that, after `grace_period`, removes the entry for
/// `hostname` from the registry IFF it's still in `Draining` state.
/// If swap_upstream was called again or the entry is otherwise Active
/// when the timer fires, this is a no-op.
pub fn spawn_drain_cleanup(state: ProxyState, hostname: String, grace_period: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(grace_period).await;
        let mut w = state.upstreams.write().await;
        w.remove_if_draining(&hostname);
    });
}
```

- [ ] **Step 4: Re-export from `proxy/mod.rs`**

Add to `crates/isengard-agent/src/proxy/mod.rs`:

```rust
pub mod swap;
pub use swap::swap_upstream;
pub use upstreams::{Upstream, UpstreamRegistry, UpstreamState};  // include UpstreamState re-export
```

- [ ] **Step 5: Update router guard**

In `crates/isengard-agent/src/proxy/router.rs`, in `IsengardProxy::upstream_peer`, add a check for the draining state:

```rust
async fn upstream_peer(
    &self,
    session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
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
    if up.state == crate::proxy::UpstreamState::Draining {
        return Err(pingora_core::Error::because(
            pingora_core::ErrorType::HTTPStatus(503),
            "upstream_draining",
            format!("upstream for {host} is draining"),
        ));
    }
    // Pingora 0.8 quirk: even with tls=false the SNI string doubles as the
    // upstream Host header. Empty SNI causes Pingora to 502 without
    // attempting the upstream connection. Pass the original public hostname.
    Ok(Box::new(HttpPeer::new(up.addr, false, host)))
}
```

(The existing comment about "Pingora 0.8 quirk" is preserved; the change is the new `if up.state == ... { return Err(...) }` block.)

- [ ] **Step 6: Run tests**

Run: `cargo test -p isengard-agent --test proxy_swap_unit`
Expected: 3 green.

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/isengard-agent/src/proxy/swap.rs \
        crates/isengard-agent/src/proxy/mod.rs \
        crates/isengard-agent/src/proxy/router.rs \
        crates/isengard-agent/tests/proxy_swap_unit.rs
git commit -m "feat(agent): swap_upstream with drain semantics + router skip-draining guard"
```

---

## Plan C wrap-up

### Task 12: Final workspace-green + open PR

- [ ] **Step 1: All four gates**

Run: `cargo build --workspace`
Expected: clean, no warnings under `-D warnings`.

Run: `cargo test --workspace`
Expected: all green (new HTTP tests + new swap tests + all existing).

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. If clippy flags Vue-adjacent generated code or anything new, fix per Plan A/B precedent (`#[allow(...)]` with rationale).

Run: `cargo deny check`
Expected: clean. New deps from this plan: probably `axum-test` (dev) — if it surfaces a new advisory, add to `deny.toml` ignore list with rationale.

Run: `cd crates/isengard-plugins/dashboard/web && bun run build`
Expected: success.

- [ ] **Step 2: Push + open PR**

```bash
git push -u origin feat/networking-settings-ui-and-swap

gh pr create --base feat/networking-tls-adapters \
  --title "Phase 8 Plan C: Settings UI Networking tab + atomic upstream swap" \
  --body "$(cat <<'EOF'
## Summary

Implements Plan C of Phase 8.

- **8h**: REST endpoints in dashboard plugin for routing rules + adapter config CRUD, including a test-connection endpoint. Frontend converts /settings to tabs (general/networking/notifier), adds a Networking tab with adapter cards (cf-tunnel, tailscale, none) + routing rules table + 2 modals (RoutingRuleEditModal, ServiceExposeModal).
- **8i**: Atomic upstream swap on the agent: `swap_upstream(hostname, new_upstream, grace_period)` with Active/Draining state machine, router skip-draining guard, drain cleanup timer.

## Spec

`docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`

## Test plan

- [x] cargo build --workspace
- [x] cargo test --workspace
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo deny check
- [x] bun run build (dashboard frontend)
- [ ] Manual smoke: /settings?tab=networking renders; create + edit + delete a routing rule
- [ ] Manual smoke: configure cf-tunnel adapter (real CF token); Test connection returns ✓
- [ ] Manual smoke: configure tailscale adapter; status pill reflects state
- [ ] Manual smoke: ServiceExposeModal opens from Stack detail with the right service prefilled

Stacked on PR #19 (Plan B). Will rebase after #18 + #19 merge.
EOF
)"
```

- [ ] **Step 3: Return PR URL**

---

## Notes for the implementer

- The dashboard plugin's existing `api.rs` may already have a Router-building convention. Match it — don't restructure beyond adding the new `routing.rs` module.
- The Vue components above use Tailwind classes that exist in the dashboard's design tokens (`iso-bg-base`, `iso-text-primary`, etc.). If a class is missing, fall back to the closest existing utility.
- The `useApi` composable (if present) may have built-in auth header injection that the raw `fetch` calls in the new composables miss — check `composables/useApi.ts` and use it if it exists.
- Pinia store paths (`~/stores/hosts`) may differ — adapt to the actual location.
- `axum-test` version: pick what's compatible with the existing axum 0.8. If the existing dashboard tests use a different test pattern (e.g. `Router::oneshot`), copy that style instead.
- The existing `[id].vue` Stack detail page may or may not have an "Expose" button surface. If it doesn't, add a placeholder per service (small action button) so the modal has a trigger.
- `bun install` may already be cached in the worktree; if not, the first build of the dashboard web bundle will pull dependencies.
