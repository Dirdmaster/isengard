# Phase 5b: REST API + WebSocket Live Events

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Dashboard backend exposes a complete read-only REST API surface (hosts, stacks, services, events, fleets) PLUS a WebSocket at `/ws/events` that broadcasts live events from the controller's EventBus to connected clients. Frontend gets a `useApi` composable and Pinia stores (`useHosts`, `useEvents`, etc.) that hydrate from REST and stay live via WS. By end of 5b: `curl http://localhost:9418/api/v1/hosts` returns JSON; `wscat -c ws://localhost:9418/ws/events` streams events as they happen.

**Architecture:** Dashboard plugin's axum router gains `/api/v1/*` handlers. Each handler reads from the shared `Inventory` + `Journal` (passed via PluginContext as opaque handles, downcast in plugin start). WebSocket handler subscribes to `EventBus` per connection, serializes events to JSON, ships frames over WS. Frontend uses Nuxt's `$fetch` (REST) and a tiny WebSocket composable (live).

**Tech stack additions:** `serde` JSON serialization for API DTOs (already a workspace dep); `axum` WebSocket support (already enabled via `ws` feature in 5a). No new deps.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-05-01-phase-5-dashboard-design.md` §8 (API surface) + §10 (real-time data flow).

---

## Scope

**In:**
- Dashboard plugin's `PluginContext` access to `Inventory`, `Journal`, `EventBus` (downcast from `bus: Option<Arc<dyn Any>>`). Phase 4d's controller plugin host already passes the bus this way; we extend the convention to also pass inventory + journal.
- New API DTOs in `crates/isengard-plugins/dashboard/src/dto.rs` — wire-format structs with `serde::Serialize`. Map from internal types (`Host`, `EventRow`, etc.) to DTOs.
- New API handlers in `crates/isengard-plugins/dashboard/src/api.rs`:
  - `GET /api/v1/hosts` — list with optional `?fleet=` and `?state=` filters
  - `GET /api/v1/hosts/:id` — single host
  - `GET /api/v1/hosts/:id/events?limit=N` — events for one host
  - `GET /api/v1/stacks` — list (with `?host_id=`, `?fleet=` filters); v1 returns synthesized stacks from container labels (since the Stack entity migration lands in 5d)
  - `GET /api/v1/stacks/:id` — single stack
  - `GET /api/v1/services` — flat list of all services (from inventory's container snapshot if available; v1 may return empty until 5d)
  - `GET /api/v1/services/:id` — single service
  - `GET /api/v1/events?limit=N&kind=&host_id=&since=` — paginated journal query
  - `GET /api/v1/events/:id` — single event
  - `GET /api/v1/fleets` — list of distinct fleet tags + host counts
- Mutation endpoints (5b scope):
  - `POST /api/v1/hosts/enroll` — returns `{enrollment_token, install_command}` with placeholder values; full enrollment flow lands in 5e
  - `PATCH /api/v1/hosts/:id` — update fleet only (other fields v1.x)
  - `DELETE /api/v1/hosts/:id` — decommission (calls `inventory.delete_host`; new method)
  - `POST /api/v1/hosts/:id/actions/force-update` — sets a flag; agent picks it up next sync (full implementation lands in 5d when force-update RPC is wired)
- New WebSocket handler in `crates/isengard-plugins/dashboard/src/ws.rs`:
  - `WS /ws/events` upgrade endpoint
  - On connect: send `welcome` frame (snapshot summary)
  - Subscribe to `EventBus` via `bus.subscribe()`
  - Loop: on each event, JSON-encode and send WS Text frame
  - Handle ping/pong for keep-alive
  - Cleanup subscription on disconnect
- Frontend additions in `web/`:
  - `composables/useApi.ts` — wraps `$fetch` with base URL handling
  - `composables/useEventStream.ts` — wraps WebSocket connection, exposes reactive event stream
  - `stores/events.ts` — Pinia store for events list (hydrates via REST, prepends via WS)
  - `stores/hosts.ts` — Pinia store for hosts list (hydrates via REST; refetches on host-relevant events)
  - `stores/fleets.ts` — Pinia store for fleet picker
- Updated placeholder home page that displays counts from the stores (proof of life)
- New unit tests:
  - DTO mapping tests
  - API handler tests via `axum::Router` + `tower::ServiceExt::oneshot` (in-process HTTP testing)
- Inventory new method: `delete_host(host_id) -> Result<bool>` for DELETE endpoint
- New context-passing convention: `PluginContext.controller_handles: Option<Arc<dyn Any>>` carries `Arc<ControllerHandles>` struct with `inventory + journal + bus`. Update Phase 4d's plugin host to construct + pass this.

**Out (deferred to 5c–5e):**
- Vue components rendering the data (just bare stores + placeholder home in 5b)
- Cmd pane (5c)
- Force-update RPC pipe (5d)
- Add host modal (5e)
- Authentication (v1.x)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline (from 5a) + new tests
3. `just ci-local` clean (cargo-deny mandatory)
4. `bun --cwd crates/isengard-plugins/dashboard/web run build` succeeds
5. Manual smoke (in 3 terminals):
   - T1: controller running with at least 1 enrolled host that has emitted some events
   - T2: `curl -s http://localhost:9418/api/v1/hosts | jq .` returns JSON array with the host
   - T3: `curl -s http://localhost:9418/api/v1/events?limit=10 | jq .` returns recent events
   - T4 (or browser): `wscat -c ws://localhost:9418/ws/events` connects, receives `welcome` frame, then receives `event` frames as the agent emits them
6. Tag `v0.1.0-alpha.phase5b` set locally
7. **Not pushed**

---

## File Structure

```
crates/isengard-controller/
└── src/
    ├── lib.rs                          # MODIFY: ControllerHandles struct, plugin_host passes it via ctx
    └── plugin_host.rs                  # MODIFY: pass ControllerHandles in PluginContext.bus

crates/isengard-storage/
└── src/inventory.rs                    # MODIFY: + delete_host method

crates/isengard-plugins/dashboard/
└── src/
    ├── lib.rs                          # MODIFY: get handles from ctx; pass to api + ws routers
    ├── dto.rs                          # NEW: API DTOs
    ├── api.rs                          # NEW: REST handlers
    └── ws.rs                           # NEW: WebSocket handler

crates/isengard-plugins/dashboard/web/
├── composables/
│   ├── useApi.ts                       # NEW
│   └── useEventStream.ts               # NEW
├── stores/
│   ├── events.ts                       # NEW
│   ├── hosts.ts                        # NEW
│   └── fleets.ts                       # NEW
└── pages/
    └── index.vue                       # MODIFY: show live counts
```

---

## Task 1: ControllerHandles + plugin_host passes them

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs`
- Modify: `crates/isengard-controller/src/plugin_host.rs`

- [ ] **Step 1: Define ControllerHandles**

In `crates/isengard-controller/src/lib.rs`, add:

```rust
use std::sync::Arc;
use isengard_storage::{Inventory, Journal};
use crate::bus::EventBus;

/// Bundle of references controller-side plugins need to access controller state.
/// Passed via `PluginContext.bus` (downcast from `Arc<dyn Any>`).
#[derive(Clone)]
pub struct ControllerHandles {
    pub inventory: Arc<Inventory>,
    pub journal: Arc<Journal>,
    pub bus: Arc<EventBus>,
}
```

- [ ] **Step 2: Plugin host passes ControllerHandles instead of just bus**

In `crates/isengard-controller/src/plugin_host.rs`, change `load_controller_plugins` signature:

```rust
pub async fn load_controller_plugins(
    handles: Arc<ControllerHandles>,
    config: Value,
) -> Vec<LoadedPlugin> {
    let handles_any: Arc<dyn Any + Send + Sync> = handles.clone();
    let ctx = PluginContext::new(HostMode::Controller, config).with_bus(handles_any);
    // ... rest unchanged, plugins now downcast to Arc<ControllerHandles>
}
```

- [ ] **Step 3: Update run_controller**

In `crates/isengard-controller/src/lib.rs`, where `load_controller_plugins(bus.clone(), journal.clone(), opts.config.clone())` is called, replace with:

```rust
let handles = Arc::new(ControllerHandles {
    inventory: inventory.clone(),
    journal: journal.clone(),
    bus: bus.clone(),
});
let mut controller_plugins = plugin_host::load_controller_plugins(handles, opts.config.clone()).await;
```

- [ ] **Step 4: Update notifier (existing controller plugin) to use ControllerHandles**

In `crates/isengard-plugins/notifier/src/lib.rs`, the `start()` method currently downcasts `ctx.bus` to `Arc<EventBus>`. Update to downcast to `Arc<ControllerHandles>` and access `.bus`:

```rust
let handles = ctx
    .bus
    .clone()
    .ok_or_else(|| start_err("notifier started without ControllerHandles on PluginContext"))?
    .downcast::<isengard_controller::ControllerHandles>()
    .map_err(|_| start_err("bus on PluginContext was not ControllerHandles"))?;
let mut rx = handles.bus.subscribe();
```

- [ ] **Step 5: Build + tests**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -10
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: clean build, all tests still pass.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller crates/isengard-plugins/notifier
cd ~/Projects/isengard && git commit -m "refactor(controller): pass ControllerHandles bundle to plugins via PluginContext"
```

---

## Task 2: Inventory delete_host

**Files:**
- Modify: `crates/isengard-storage/src/inventory.rs`

- [ ] **Step 1: Add delete_host method**

Append to `Inventory`:

```rust
/// Remove a host from the inventory. Returns true if a row was deleted.
pub async fn delete_host(&self, id: HostId) -> Result<bool, Error> {
    let bytes = id.to_bytes().to_vec();
    let result = sqlx::query("DELETE FROM hosts WHERE id = ?")
        .bind(bytes)
        .execute(&self.pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
```

- [ ] **Step 2: Add unit test**

In `mod tests` block in inventory.rs:

```rust
#[tokio::test]
async fn delete_host_removes_entry() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let enroll = EnrollHost {
        fingerprint: "fp-delete".into(),
        hostname: "h1".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "test".into(),
        docker_version: "test".into(),
    };
    let id = inv.enroll_host(enroll).await.unwrap();
    let removed = inv.delete_host(id).await.unwrap();
    assert!(removed);
    assert!(inv.get_host(id).await.unwrap().is_none());
    let removed_again = inv.delete_host(id).await.unwrap();
    assert!(!removed_again);
}
```

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-storage delete_host 2>&1 | tail -10
```

Expected: pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-storage/src/inventory.rs
cd ~/Projects/isengard && git commit -m "feat(storage): Inventory::delete_host (used by dashboard DELETE endpoint)"
```

---

## Task 3: Dashboard DTOs

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/dto.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs` (add `mod dto;`)

- [ ] **Step 1: Create dto.rs**

```rust
//! Wire-format DTOs for the dashboard REST API.

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_storage::{EventRow, Host, HostId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct HostDto {
    pub id: String,
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
    pub fleet: String,
    pub enrolled_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

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
            fleet: "default".to_string(), // 5d migration adds the real field
            enrolled_at: h.enrolled_at,
            last_seen_at: h.last_seen_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EventDto {
    pub id: i64,
    pub kind: String,
    pub host_id: Option<String>,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl From<EventRow> for EventDto {
    fn from(r: EventRow) -> Self {
        let metadata = r
            .metadata_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        Self {
            id: r.id,
            kind: r.kind,
            host_id: r.host_id.map(|h| ulid::Ulid::from(h).to_string()),
            container_name: r.container_name,
            image: r.image,
            old_digest: r.old_digest,
            new_digest: r.new_digest,
            error: r.error,
            summary: r.summary,
            metadata,
            occurred_at: r.occurred_at,
            received_at: r.received_at,
        }
    }
}

/// Live event frame for WebSocket broadcasts.
#[derive(Debug, Clone, Serialize)]
pub struct LiveEventDto {
    pub kind: String,
    pub host_id: Option<String>,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
}

impl From<&Event> for LiveEventDto {
    fn from(e: &Event) -> Self {
        Self {
            kind: e.kind.clone(),
            host_id: e.host_id.map(|h| h.to_string()),
            container_name: e.container_name.clone(),
            image: e.image.clone(),
            summary: e.summary.clone(),
            occurred_at: e.occurred_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetDto {
    pub name: String,
    pub host_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub fleet: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub enrollment_token: String,
    pub install_command: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchHostRequest {
    pub fleet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    pub limit: Option<i64>,
    pub kind: Option<String>,
    pub host_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HostsQuery {
    pub fleet: Option<String>,
    pub state: Option<String>,
}
```

In `crates/isengard-plugins/dashboard/src/lib.rs`, add at top:

```rust
mod dto;
```

- [ ] **Step 2: Build + test**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-dashboard 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-dashboard --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/src/dto.rs crates/isengard-plugins/dashboard/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(dashboard): API DTOs (HostDto, EventDto, LiveEventDto, etc.)"
```

---

## Task 4: REST API handlers

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/api.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs` (mount API router)

- [ ] **Step 1: Create api.rs**

```rust
//! REST API handlers for the dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use isengard_controller::ControllerHandles;
use isengard_storage::HostId;
use serde_json::json;
use tracing::warn;

use crate::dto::*;

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/hosts", get(list_hosts).post(enroll_host))
        .route("/hosts/{id}", get(get_host).patch(patch_host).delete(delete_host))
        .route("/hosts/{id}/events", get(host_events))
        .route("/events", get(list_events))
        .route("/events/{id}", get(get_event))
        .route("/fleets", get(list_fleets))
        .with_state(handles)
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn list_hosts(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<HostsQuery>,
) -> Response {
    match handles.inventory.list_hosts().await {
        Ok(rows) => {
            let mut dtos: Vec<HostDto> = rows.into_iter().map(HostDto::from).collect();
            // 5d implements real fleet field; for now filter is a no-op unless "default"
            if let Some(fleet) = q.fleet {
                dtos.retain(|h| h.fleet == fleet);
            }
            Json(dtos).into_response()
        }
        Err(e) => {
            warn!(error = %e, "list_hosts failed");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_hosts: {e}"))
        }
    }
}

async fn get_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    match handles.inventory.get_host(host_id).await {
        Ok(Some(h)) => Json(HostDto::from(h)).into_response(),
        Ok(None) => json_err(StatusCode::NOT_FOUND, "host not found"),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_host: {e}")),
    }
}

async fn host_events(
    State(handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match handles.journal.list_recent(limit).await {
        Ok(rows) => {
            // 5b returns recent events; future: filter by host_id at SQL level
            let dtos: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("host_events: {e}")),
    }
}

async fn enroll_host(
    State(_handles): State<Arc<ControllerHandles>>,
    Json(_req): Json<EnrollRequest>,
) -> Response {
    // 5e wires real enrollment with a token store. For 5b, return a placeholder.
    let token = format!("pending-{}", ulid::Ulid::new());
    let cmd = format!(
        "docker run -d --name isengard-agent -e ISENGARD_TOKEN={token} ghcr.io/dirdmaster/isengard:latest agent --controller=https://CONTROLLER_HOST:9417"
    );
    Json(EnrollResponse {
        enrollment_token: token,
        install_command: cmd,
    })
    .into_response()
}

async fn patch_host(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
    Json(_req): Json<PatchHostRequest>,
) -> Response {
    // 5d adds real fleet column to hosts; until then, no-op success.
    Json(json!({ "ok": true })).into_response()
}

async fn delete_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    match handles.inventory.delete_host(host_id).await {
        Ok(deleted) => Json(DeleteResponse { deleted }).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("delete_host: {e}")),
    }
}

async fn list_events(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match handles.journal.list_recent(limit).await {
        Ok(rows) => {
            let mut dtos: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
            if let Some(kind) = q.kind {
                dtos.retain(|e| e.kind == kind);
            }
            if let Some(host_id_s) = q.host_id {
                dtos.retain(|e| e.host_id.as_deref() == Some(&host_id_s));
            }
            Json(dtos).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_events: {e}")),
    }
}

async fn get_event(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id_str): Path<String>,
) -> Response {
    let id: i64 = match id_str.parse() {
        Ok(i) => i,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid event id"),
    };
    match handles.journal.list_recent(500).await {
        Ok(rows) => match rows.into_iter().find(|r| r.id == id) {
            Some(r) => Json(EventDto::from(r)).into_response(),
            None => json_err(StatusCode::NOT_FOUND, "event not found"),
        },
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_event: {e}")),
    }
}

async fn list_fleets(State(handles): State<Arc<ControllerHandles>>) -> Response {
    match handles.inventory.list_hosts().await {
        Ok(rows) => {
            // 5d: aggregate by real fleet column. For now just one default.
            let host_count = rows.len();
            let dtos = vec![FleetDto {
                name: "default".into(),
                host_count,
            }];
            Json(dtos).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_fleets: {e}")),
    }
}

fn parse_host_id(s: &str) -> Result<HostId, String> {
    let ulid = ulid::Ulid::from_string(s).map_err(|e| format!("invalid host id: {e}"))?;
    Ok(HostId::from(ulid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use isengard_controller::bus::EventBus;
    use isengard_storage::{EnrollHost, Inventory, Journal};
    use tower::ServiceExt;

    async fn test_handles() -> Arc<ControllerHandles> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let jrnl = Arc::new(Journal::open_in_memory().await.unwrap());
        let bus = Arc::new(EventBus::new());
        Arc::new(ControllerHandles {
            inventory: inv,
            journal: jrnl,
            bus,
        })
    }

    #[tokio::test]
    async fn list_hosts_empty_returns_empty_array() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(Request::builder().uri("/hosts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_hosts_returns_enrolled() {
        let handles = test_handles().await;
        handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-test".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        let app = router(handles);
        let resp = app
            .oneshot(Request::builder().uri("/hosts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["hostname"], "h1");
    }

    #[tokio::test]
    async fn list_events_returns_empty_for_fresh_journal() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn delete_host_removes() {
        let handles = test_handles().await;
        let id = handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-del".into(),
                hostname: "h2".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        let app = router(handles.clone());
        let id_str = ulid::Ulid::from(id).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/hosts/{id_str}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(handles.inventory.get_host(id).await.unwrap().is_none());
    }
}
```

In `lib.rs`, add `mod api;` and mount it under `/api/v1`:

```rust
mod api;

// In build_router (replace the existing function):
fn build_router(handles: Option<Arc<ControllerHandles>>) -> Router {
    let mut router = Router::new()
        .route("/_nuxt/{*path}", get(serve_asset_nuxt))
        .route("/", get(serve_index));

    if let Some(h) = handles {
        router = router.nest("/api/v1", api::router(h));
    }

    router.fallback(get(fallback_handler))
}
```

Update Plugin's `start()` to capture handles + pass to router:

```rust
async fn start(&mut self, ctx: &PluginContext) -> Result<()> {
    let handles = ctx
        .bus
        .clone()
        .and_then(|b| b.downcast::<isengard_controller::ControllerHandles>().ok());

    if handles.is_none() {
        warn!("dashboard started without ControllerHandles — API routes will not be mounted");
    }

    let app = build_router(handles);
    // ... rest unchanged
}
```

- [ ] **Step 2: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-dashboard 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-dashboard 2>&1 | tail -15
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-dashboard --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. 4 new API tests pass.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/src/api.rs crates/isengard-plugins/dashboard/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(dashboard): REST API handlers for hosts/events/fleets (4 unit tests)"
```

---

## Task 5: WebSocket /ws/events handler

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/ws.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs` (mount /ws route)

- [ ] **Step 1: Create ws.rs**

```rust
//! WebSocket handler for live event streaming.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use isengard_controller::ControllerHandles;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::dto::LiveEventDto;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsFrame {
    Welcome { event_count_today: usize },
    Event { event: LiveEventDto },
    Pong,
    Error { message: String },
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<ControllerHandles>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, handles))
}

async fn handle_socket(mut socket: WebSocket, handles: Arc<ControllerHandles>) {
    // Send welcome frame.
    let count_today = handles
        .journal
        .list_recent(500)
        .await
        .map(|r| r.len())
        .unwrap_or(0);
    let welcome = WsFrame::Welcome {
        event_count_today: count_today,
    };
    if let Err(e) = send_frame(&mut socket, &welcome).await {
        debug!(error = %e, "failed to send welcome");
        return;
    }

    let mut rx = handles.bus.subscribe();
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            event_result = rx.recv() => {
                match event_result {
                    Ok(event) => {
                        let dto = LiveEventDto::from(&event);
                        let frame = WsFrame::Event { event: dto };
                        if let Err(e) = send_frame(&mut socket, &frame).await {
                            debug!(error = %e, "ws send failed; closing");
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(skipped = n, "dashboard ws lagged");
                    }
                    Err(RecvError::Closed) => {
                        debug!("event bus closed");
                        break;
                    }
                }
            }
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    debug!("ws ping failed; closing");
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("ws closed by client");
                        break;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Text(_))) => {
                        // v1 doesn't accept client text frames; ignore.
                    }
                    Some(Err(e)) => {
                        debug!(error = %e, "ws recv error");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &WsFrame) -> anyhow::Result<()> {
    let json = serde_json::to_string(frame)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}
```

In `lib.rs`, add `mod ws;` and mount `/ws/events`:

```rust
mod ws;

// In build_router, after the api nest:
if let Some(h) = handles_for_ws {
    router = router.route("/ws/events", get(ws::ws_handler).with_state(h));
}
```

(Adjust pattern: clone handles before passing to api so it's also available for ws.)

Cleaner: refactor build_router to take `Option<Arc<ControllerHandles>>` and clone internally.

```rust
fn build_router(handles: Option<Arc<ControllerHandles>>) -> Router {
    let mut router = Router::new()
        .route("/_nuxt/{*path}", get(serve_asset_nuxt))
        .route("/", get(serve_index));

    if let Some(h) = handles {
        router = router
            .nest("/api/v1", api::router(h.clone()))
            .route("/ws/events", get(ws::ws_handler))
            .with_state(h);
    }

    router.fallback(get(fallback_handler))
}
```

(Note: `with_state` at the end applies state to ALL routes. The `/_nuxt`, `/`, fallback handlers don't use state. axum handles this via `()` state for stateless handlers — verify with cargo build that signatures align.)

- [ ] **Step 2: Build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-dashboard 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-dashboard --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. The state-typing might require adjustments — most likely tour the build error and fix the route signatures so non-state handlers stay generic.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/src/ws.rs crates/isengard-plugins/dashboard/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(dashboard): WebSocket /ws/events handler subscribed to EventBus"
```

---

## Task 6: Frontend composables + Pinia stores

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useApi.ts`
- Create: `crates/isengard-plugins/dashboard/web/composables/useEventStream.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/events.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/hosts.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/fleets.ts`
- Modify: `crates/isengard-plugins/dashboard/web/pages/index.vue`

- [ ] **Step 1: Create useApi.ts**

```typescript
const API_BASE = '/api/v1'

export function useApi() {
  return {
    get<T>(path: string, query?: Record<string, any>) {
      return $fetch<T>(`${API_BASE}${path}`, { query })
    },
    post<T>(path: string, body?: any) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'POST', body })
    },
    patch<T>(path: string, body?: any) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'PATCH', body })
    },
    delete<T>(path: string) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'DELETE' })
    },
  }
}
```

- [ ] **Step 2: Create useEventStream.ts**

```typescript
import { ref, onMounted, onBeforeUnmount } from 'vue'

export interface LiveEvent {
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
}

export function useEventStream() {
  const connected = ref(false)
  const events = ref<LiveEvent[]>([])
  let socket: WebSocket | null = null

  function connect() {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${proto}//${window.location.host}/ws/events`
    socket = new WebSocket(url)
    socket.addEventListener('open', () => { connected.value = true })
    socket.addEventListener('close', () => { connected.value = false })
    socket.addEventListener('message', (msg) => {
      try {
        const frame = JSON.parse(msg.data)
        if (frame.type === 'event') {
          events.value.unshift(frame.event)
          // Cap memory.
          if (events.value.length > 500) events.value.length = 500
        }
      } catch { /* ignore */ }
    })
  }

  onMounted(connect)
  onBeforeUnmount(() => { socket?.close() })

  return { connected, events }
}
```

- [ ] **Step 3: Create stores/events.ts**

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface EventRow {
  id: number
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
}

export const useEventsStore = defineStore('events', () => {
  const events = ref<EventRow[]>([])
  const loading = ref(false)
  const api = useApi()

  async function load(limit = 100) {
    loading.value = true
    try {
      events.value = await api.get<EventRow[]>('/events', { limit })
    } finally {
      loading.value = false
    }
  }

  function prepend(event: any) {
    events.value.unshift({
      id: event.id ?? -1,
      kind: event.kind,
      host_id: event.host_id,
      container_name: event.container_name,
      image: event.image,
      summary: event.summary,
      occurred_at: event.occurred_at,
    })
    if (events.value.length > 500) events.value.length = 500
  }

  return { events, loading, load, prepend }
})
```

- [ ] **Step 4: Create stores/hosts.ts**

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface Host {
  id: string
  fingerprint: string
  hostname: string
  fleet: string
  enrolled_at: string
  last_seen_at: string | null
}

export const useHostsStore = defineStore('hosts', () => {
  const hosts = ref<Host[]>([])
  const loading = ref(false)
  const api = useApi()

  async function load(fleet?: string) {
    loading.value = true
    try {
      hosts.value = await api.get<Host[]>('/hosts', fleet ? { fleet } : undefined)
    } finally {
      loading.value = false
    }
  }

  return { hosts, loading, load }
})
```

- [ ] **Step 5: Create stores/fleets.ts**

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface Fleet {
  name: string
  host_count: number
}

export const useFleetsStore = defineStore('fleets', () => {
  const fleets = ref<Fleet[]>([])
  const active = ref<string | null>(null)
  const api = useApi()

  async function load() {
    fleets.value = await api.get<Fleet[]>('/fleets')
    if (active.value === null && fleets.value.length > 0) {
      active.value = 'all'
    }
  }

  function setActive(name: string) {
    active.value = name
  }

  return { fleets, active, load, setActive }
})
```

- [ ] **Step 6: Update pages/index.vue to show live counts**

Replace pages/index.vue with:

```vue
<template>
  <div class="p-8 max-w-4xl mx-auto">
    <header class="flex items-center gap-3 mb-8">
      <div class="w-3 h-3 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-error'"></div>
      <h1 class="text-2xl font-semibold tracking-tight">Isengard Dashboard</h1>
      <span class="text-iso-text-faint text-iso-sm">Phase 5b · API + WS</span>
    </header>

    <section class="grid grid-cols-3 gap-4 mb-8">
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Hosts</div>
        <div class="text-2xl font-mono">{{ hostsStore.hosts.length }}</div>
      </div>
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Events (recent)</div>
        <div class="text-2xl font-mono">{{ eventsStore.events.length }}</div>
      </div>
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Live stream</div>
        <div class="text-2xl font-mono" :class="connected ? 'text-iso-success' : 'text-iso-error'">
          {{ connected ? 'connected' : 'offline' }}
        </div>
      </div>
    </section>

    <section v-if="liveEvents.length > 0">
      <h2 class="text-iso-sm uppercase tracking-wider text-iso-text-faint mb-3">Live (last 10)</h2>
      <ul class="space-y-1 font-mono text-iso-xs">
        <li v-for="(e, i) in liveEvents.slice(0, 10)" :key="i" class="text-iso-text-secondary">
          <span class="text-iso-text-faint">{{ new Date(e.occurred_at).toLocaleTimeString() }}</span>
          <span class="ml-2 text-iso-success">{{ e.kind }}</span>
          <span class="ml-2">{{ e.summary }}</span>
        </li>
      </ul>
    </section>
  </div>
</template>

<script setup lang="ts">
const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const { connected, events: liveEvents } = useEventStream()

await Promise.all([
  eventsStore.load(),
  hostsStore.load(),
])
</script>
```

- [ ] **Step 7: Build the bundle**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web
bun run build 2>&1 | tail -10
```

Expected: success.

- [ ] **Step 8: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): composables + Pinia stores + live counts on home page"
```

---

## Task 7: CI gate + manual smoke + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 5b`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: previous baseline + 4 API tests + 1 inventory delete_host = 5+ new tests.

- [ ] **Step 3: Manual smoke (3 terminals)**

T1 (controller + agent that emits events):
```bash
mkdir -p /tmp/isengard-5b-ctrl /tmp/isengard-5b-agent
cd ~/Projects/isengard
ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9417 --state-dir /tmp/isengard-5b-ctrl &
sleep 1
ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9417 --state-dir /tmp/isengard-5b-agent &
```

T2 (REST):
```bash
curl -s http://localhost:9418/api/v1/hosts | jq .
curl -s http://localhost:9418/api/v1/events?limit=10 | jq .
curl -s http://localhost:9418/api/v1/fleets | jq .
```

Expected: hosts returns the enrolled host. Events returns recent events (heartbeats won't show up — only updater events). Fleets returns one entry "default".

T3 (browser):
- Open http://localhost:9418/
- See "Isengard Dashboard" header with green "connected" indicator
- Three stat tiles: Hosts: 1, Events: N, Live stream: connected
- If you trigger an event (e.g., add a labelled container that the updater notices), see it appear in the "Live (last 10)" section in real time

- [ ] **Step 4: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase5b -m "phase 5b: REST API + WebSocket live events + Pinia stores"
```

- [ ] **Step 5: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` zero failures
- [ ] `just ci-local` clean
- [ ] Manual smoke confirms REST + WS working
- [ ] Tag `v0.1.0-alpha.phase5b` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§8 + §10) | Plan task |
|---|---|
| ControllerHandles bundle for plugin context | Task 1 |
| `Inventory::delete_host` | Task 2 |
| API DTOs | Task 3 |
| REST handlers (hosts/events/fleets) | Task 4 |
| WebSocket `/ws/events` | Task 5 |
| Frontend composables + Pinia stores | Task 6 |
| Live event flow end-to-end | Task 7 (manual smoke) |

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-05-01-phase-5b-api-and-ws.md`. Subagent-driven execution.
