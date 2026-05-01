# Phase 5e — Events + Settings + Add-host Modal + Cmd Pane Terminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the four remaining v1 surfaces — Events tab, Settings page, Add-host modal, and Cmd pane terminal mode — and the supporting backend (services persistence, force-update RPC pipe, WS log streaming).

**Architecture:** A new `services` table closes the gap left in 5d so `/api/v1/services` returns real rows. A new `host_actions` queue lets the dashboard push force-update directives that the agent picks up on its next sync. A new WebSocket endpoint `/api/v1/services/:id/logs` proxies `docker logs -f` from the agent through the controller to the browser via xterm.js. The frontend gains: a fully filterable Events page, a Settings page with fleets CRUD and notifier config, an Add-host modal that displays the curl install command from the `enroll` endpoint, and a CmdTerminal sub-component with xterm.js + breadcrumb header that mounts inside the existing CmdPane shell.

**Tech Stack:** Rust (sqlx 0.8, axum 0.8, tonic 0.13, tokio-tungstenite for WS proxy), TypeScript / Vue 3 / Nuxt 3 / xterm.js / Tailwind 4 / Pinia.

---

## Scope

**In:**
- New migration `crates/isengard-storage/migrations/0005_services.sql`:
  - `services` table: `id INTEGER AI`, `host_id BLOB FK`, `stack_id INTEGER NULL FK`, `name TEXT`, `image TEXT`, `state TEXT`, `last_seen_at TEXT`, `UNIQUE(host_id, name)`
- New migration `crates/isengard-storage/migrations/0006_host_actions.sql`:
  - `host_actions` table: `id INTEGER AI`, `host_id BLOB FK`, `kind TEXT`, `payload_json TEXT`, `created_at`, `delivered_at NULL`, `result TEXT NULL`
- Storage extensions in `crates/isengard-storage/src/`:
  - New `service.rs` with `ServiceId`, `Service`, `InsertService`
  - New `host_action.rs` with `HostActionId`, `HostAction`, `HostActionKind` enum (`ForceUpdate { stack_name?: String }`, `Decommission`, etc.)
  - `Inventory::insert_service(InsertService) -> Result<ServiceId>` (upsert by `(host_id, name)`)
  - `Inventory::list_services(Option<StackId>) -> Result<Vec<Service>>`
  - `Inventory::get_service(ServiceId) -> Result<Option<Service>>`
  - `Inventory::queue_action(HostId, HostActionKind) -> Result<HostActionId>`
  - `Inventory::pending_actions(HostId) -> Result<Vec<HostAction>>`
  - `Inventory::mark_action_delivered(HostActionId, &str) -> Result<()>`
- Proto extension (`crates/isengard-proto/proto/sync.proto`):
  - `message ServiceInfo { string name = 1; string image = 2; string state = 3; optional string stack = 4; }`
  - `Heartbeat` (or `InventorySnapshot`) gains `repeated ServiceInfo services = N;`
  - `SyncResponse` (the controller's reply to a heartbeat) gains `repeated PendingAction pending_actions = N;` where `PendingAction { uint64 id = 1; string kind = 2; string payload_json = 3; }`
  - Or: a dedicated `Stream<PendingAction>` if the existing model is push-based — pick whichever the existing transport uses
- Agent extensions:
  - On inventory snapshot, build `Vec<ServiceInfo>` from container list (one per running/stopped container) including image and state
  - Process `pending_actions` from the controller's response: dispatch each to the existing updater (force-update) or other handlers
  - Emit `dashboard.action_dispatched` event when an action runs (success or failure)
- Controller extensions in `crates/isengard-controller/src/sync.rs`:
  - On heartbeat: persist services via `inventory.insert_service` (with prune analogous to stacks)
  - On heartbeat reply: pull `inventory.pending_actions(host_id)`, attach to response, mark each delivered
- New WebSocket endpoint in `crates/isengard-plugins/dashboard/src/ws.rs`:
  - `WS /api/v1/services/:id/logs` — opens a bidirectional WS to the browser
  - Handler resolves service → host → opens an internal RPC stream to the agent (`AgentControl::stream_logs(service_name)`)
  - Streams stdout/stderr lines as WS Text frames (`{"type":"line","stream":"stdout","text":"..."}`)
  - Browser sends `{"type":"close"}` to terminate
  - For v1 this is **read-only logs**. Bidirectional shell exec (`docker exec -it`) is deferred to v1.x — call out explicitly in the spec README delta.
- New REST endpoints in `crates/isengard-plugins/dashboard/src/api.rs`:
  - `POST /api/v1/hosts/:id/actions/force-update` — replaces the 5b stub; queues a `ForceUpdate { stack_name: None }` action
  - `POST /api/v1/stacks/:id/actions/force-update` — replaces the 5d stub; queues `ForceUpdate { stack_name: Some(...) }`
  - `POST /api/v1/hosts/enroll` — replaces the 5b placeholder; generates real enrollment token + returns curl install command
  - `GET /api/v1/fleets` — already exists; verify it returns real counts (5b stub may have returned `[]`)
  - `POST /api/v1/fleets` — register a fleet tag (just records intent — fleets are derived from `hosts.fleet`, but we keep a `fleets` table for empty fleets)
  - `DELETE /api/v1/fleets/:tag` — delete a fleet tag if no hosts assigned
  - `GET /api/v1/settings` — returns the controller config snapshot (notifier endpoints, agent install instructions, etc.)
  - `PATCH /api/v1/settings` — updates a subset of settings (notifier toggles, default fleet)
  - `GET /install.sh?token=<t>` — serves the agent installer bash script bound to a one-time enrollment token (validates token before serving)
- New migration `crates/isengard-storage/migrations/0007_fleets.sql` and `0008_settings.sql`:
  - `fleets` table (just `name TEXT PRIMARY KEY`, `created_at TEXT`)
  - `settings` table (key/value JSON store)
- Frontend pages and components:
  - `pages/events/index.vue` — full Events page with filter chips (kind, host, stack, since)
  - `pages/events/[id].vue` — single event detail (rare deep-link)
  - `pages/settings/index.vue` — Settings page with three sections (Fleets, Notifiers, Agent enrollment)
  - `components/EventFilterChip.vue` — toggleable filter chip
  - `components/EventsPage.vue` — composes EventTimeline (from 5c) with filter bar
  - `components/AddHostModal.vue` — modal with form + curl command output
  - `components/SettingsSection.vue` — collapsible section primitive
  - `components/FleetsSettings.vue` — list/create/delete fleets
  - `components/NotifierSettings.vue` — toggle Telegram/Discord, show config link
  - `components/EnrollmentSettings.vue` — explain enrollment, button to open modal
  - `components/CmdTerminal.vue` — xterm.js mounted inside CmdPane in terminal mode
  - `components/CmdBreadcrumb.vue` — header for terminal mode showing context path
  - `components/Toast.vue` + `components/ToastContainer.vue` — bottom-right toast notifications for action feedback
  - `components/ConfirmDialog.vue` — in-app destructive-action confirm (replaces native `confirm()` for decommission, delete fleet)
  - `composables/useLogStream.ts` — WS wrapper for `/api/v1/services/:id/logs`
  - `composables/useToast.ts` — convenience wrapper around toasts store
  - `composables/useConfirm.ts` — promise-returning confirm dialog
  - `stores/settings.ts` — Pinia for settings
  - `stores/fleets.ts` — extend (was 5b stub) with create/delete actions
  - `stores/toasts.ts` — Pinia for the toast queue
  - `templates/install.sh.tmpl` — bash installer template rendered by `/install.sh` handler
- New unit + integration tests:
  - Migration tests (services, host_actions, fleets, settings)
  - `Inventory::queue_action` + `pending_actions` + `mark_action_delivered` round-trip
  - Service upsert + prune
  - DTO tests for new shapes
  - API handler tests for force-update queueing, fleet CRUD, enrollment
  - WS log streaming integration test (mock agent via in-process channel)

**Out (deferred to v1.x):**
- Bidirectional shell (`docker exec -i`) — read-only logs only in v1
- Custom widget dashboards
- Multi-select bulk actions
- OIDC / SSO authentication
- Audit log of user actions (we emit `dashboard.action_dispatched` events but don't yet display an audit page)
- Mobile breakpoint
- AI chat in cmd pane
- LLM-powered help on `?` prefix

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes
3. `just ci-local` clean
4. `bun --cwd crates/isengard-plugins/dashboard/web run build` produces a static bundle including the new pages + xterm.js
5. End-to-end smoke (4 terminals):
   - T1: controller running with at least 1 host enrolled and 1 compose project running
   - Browser: `/events` shows the timeline with filter chips; toggle "FAILED" → only FAILED events visible; clear filter → all visible
   - Browser: `/settings` shows Fleets section listing existing fleets + a "+ New fleet" form; create a fleet → it appears; delete it → it vanishes
   - Browser: `/hosts` → click "+ Add host" → modal opens with a curl command; copy it
   - Browser: `⌘K` → type `logs web` → select "Tail logs on web @ host" → cmd pane swaps to terminal mode showing live `docker logs -f web` output via xterm
   - Browser: click "Force update" on a host → action is queued → toast appears bottom-right → next agent heartbeat picks it up → updater runs → events flow back to the timeline
   - Browser: click Decommission in HostInspector → ConfirmDialog appears with red "Decommission" button → confirming runs the action and shows a success toast
   - Browser: copy the curl command from Add host modal, run it in a terminal pointed at the controller URL → `/install.sh` returns a valid bash script that downloads and registers the agent (smoke check via `curl ...?token=<token>` and inspect output begins with `#!/usr/bin/env bash`)
6. Tag `v0.1.0-alpha.phase5e` set locally
7. Tag `v0.1.0-alpha.phase5-complete` set locally (Phase 5 done)
8. **Not pushed**

---

## File Structure

```
crates/isengard-storage/
├── migrations/
│   ├── 0005_services.sql            # NEW
│   ├── 0006_host_actions.sql        # NEW
│   ├── 0007_fleets.sql              # NEW
│   └── 0008_settings.sql            # NEW
└── src/
    ├── lib.rs                       # MODIFY: + service, + host_action, + fleet, + setting modules
    ├── service.rs                   # NEW
    ├── host_action.rs               # NEW
    ├── fleet.rs                     # NEW
    ├── setting.rs                   # NEW
    └── inventory.rs                 # MODIFY: service + host_action + fleet + setting methods

crates/isengard-proto/
└── proto/sync.proto                 # MODIFY: + ServiceInfo, + PendingAction, + Heartbeat fields

crates/isengard-agent/
└── src/
    ├── inventory_snapshot.rs        # MODIFY: + ServiceInfo derivation
    └── sync.rs                      # MODIFY: process pending_actions, emit dispatch events

crates/isengard-controller/
└── src/sync.rs                      # MODIFY: persist services, attach pending_actions to reply

crates/isengard-plugins/dashboard/
├── src/
│   ├── dto.rs                       # MODIFY: + FleetDto (real), + SettingsDto, + EnrollmentDto
│   ├── api.rs                       # MODIFY: real force-update + enroll + fleet CRUD + settings + install_sh
│   └── ws.rs                        # MODIFY: + handle_logs WS handler
└── templates/
    └── install.sh.tmpl              # NEW (rendered by /install.sh handler)

crates/isengard-plugins/dashboard/web/
├── components/
│   ├── EventFilterChip.vue          # NEW
│   ├── EventsPage.vue               # NEW (the composed view; pages/events/index uses it)
│   ├── AddHostModal.vue             # NEW
│   ├── SettingsSection.vue          # NEW
│   ├── FleetsSettings.vue           # NEW
│   ├── NotifierSettings.vue         # NEW
│   ├── EnrollmentSettings.vue       # NEW
│   ├── CmdTerminal.vue              # NEW
│   ├── CmdBreadcrumb.vue            # NEW
│   ├── Toast.vue                    # NEW
│   ├── ToastContainer.vue           # NEW
│   ├── ConfirmDialog.vue            # NEW
│   └── AddHostButton.vue            # MODIFY (5d): wire to AddHostModal
├── composables/
│   ├── useLogStream.ts              # NEW
│   ├── useToast.ts                  # NEW
│   └── useConfirm.ts                # NEW
├── layouts/
│   └── default.vue                  # NEW (mounts ToastContainer + ConfirmDialog)
├── stores/
│   ├── settings.ts                  # NEW
│   ├── fleets.ts                    # MODIFY (5b extension): create/delete actions
│   └── toasts.ts                    # NEW
└── pages/
    ├── events/
    │   ├── index.vue                # NEW
    │   └── [id].vue                 # NEW
    └── settings/
        └── index.vue                # NEW
```

---

## Task 1: Storage migrations + models for services, host_actions, fleets, settings

**Files:**
- Create: `crates/isengard-storage/migrations/0005_services.sql`
- Create: `crates/isengard-storage/migrations/0006_host_actions.sql`
- Create: `crates/isengard-storage/migrations/0007_fleets.sql`
- Create: `crates/isengard-storage/migrations/0008_settings.sql`
- Create: `crates/isengard-storage/src/service.rs`
- Create: `crates/isengard-storage/src/host_action.rs`
- Create: `crates/isengard-storage/src/fleet.rs`
- Create: `crates/isengard-storage/src/setting.rs`
- Modify: `crates/isengard-storage/src/lib.rs`

- [ ] **Step 1: Write migration 0005_services.sql**

```sql
CREATE TABLE services (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id       BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id      INTEGER          REFERENCES stacks(id) ON DELETE SET NULL,
    name          TEXT    NOT NULL,
    image         TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK(state IN ('running', 'stopped', 'restarting', 'unknown')),
    last_seen_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(host_id, name)
);
CREATE INDEX idx_services_host_id  ON services(host_id);
CREATE INDEX idx_services_stack_id ON services(stack_id);
```

- [ ] **Step 2: Write migration 0006_host_actions.sql**

```sql
CREATE TABLE host_actions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id       BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL,
    payload_json  TEXT    NOT NULL DEFAULT '{}',
    created_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at  TEXT,
    result        TEXT
);
CREATE INDEX idx_host_actions_host_id_pending ON host_actions(host_id, delivered_at);
```

- [ ] **Step 3: Write migration 0007_fleets.sql**

```sql
CREATE TABLE fleets (
    name        TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT OR IGNORE INTO fleets (name) VALUES ('default');
```

- [ ] **Step 4: Write migration 0008_settings.sql**

```sql
CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value_json  TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

- [ ] **Step 5: Write the service.rs model**

```rust
//! Service entity: one container belonging to a host (and optionally a stack).

use crate::host::HostId;
use crate::stack::StackId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(pub i64);

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Running,
    Stopped,
    Restarting,
    Unknown,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Restarting => "restarting",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "restarting" => Self::Restarting,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub id: ServiceId,
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertService {
    pub host_id: HostId,
    pub stack_id: Option<StackId>,
    pub name: String,
    pub image: String,
    pub state: ServiceState,
}
```

- [ ] **Step 6: Write the host_action.rs model**

```rust
//! Queued action for a host. The agent pulls these on its next heartbeat.

use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostActionId(pub i64);

impl std::fmt::Display for HostActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostActionKind {
    ForceUpdate { stack_name: Option<String> },
    Decommission,
}

impl HostActionKind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::ForceUpdate { .. } => "force_update",
            Self::Decommission => "decommission",
        }
    }

    pub fn payload_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    pub id: HostActionId,
    pub host_id: HostId,
    pub kind: HostActionKind,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub result: Option<String>,
}
```

- [ ] **Step 7: Write the fleet.rs model**

```rust
//! Fleet: a user-defined tag for grouping hosts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    pub name: String,
    pub created_at: DateTime<Utc>,
}
```

- [ ] **Step 8: Write the setting.rs model**

```rust
//! Settings: simple key/value store of JSON-serialized config values.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}
```

- [ ] **Step 9: Re-export from lib.rs**

```rust
pub mod service;
pub mod host_action;
pub mod fleet;
pub mod setting;

pub use service::{InsertService, Service, ServiceId, ServiceState};
pub use host_action::{HostAction, HostActionId, HostActionKind};
pub use fleet::Fleet;
pub use setting::Setting;
```

- [ ] **Step 10: Run a build to confirm migrations compile**

Run: `cargo build -p isengard-storage`
Expected: PASS — no test failures yet because the inventory methods aren't there. Migration files are picked up by sqlx::migrate! at runtime.

- [ ] **Step 11: Commit**

```bash
git add crates/isengard-storage/migrations/000{5,6,7,8}_*.sql \
        crates/isengard-storage/src/service.rs \
        crates/isengard-storage/src/host_action.rs \
        crates/isengard-storage/src/fleet.rs \
        crates/isengard-storage/src/setting.rs \
        crates/isengard-storage/src/lib.rs
git commit -m "feat(storage): + services, host_actions, fleets, settings tables + models"
```

---

## Task 2: Inventory methods for services, host_actions, fleets, settings

**Files:**
- Modify: `crates/isengard-storage/src/inventory.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn insert_service_upserts_by_host_and_name() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = test_enroll(&inv, "h1").await;

    let id1 = inv.insert_service(InsertService {
        host_id, stack_id: None, name: "web".into(), image: "nginx:1".into(),
        state: ServiceState::Running,
    }).await.unwrap();

    let id2 = inv.insert_service(InsertService {
        host_id, stack_id: None, name: "web".into(), image: "nginx:2".into(),
        state: ServiceState::Restarting,
    }).await.unwrap();

    assert_eq!(id1, id2);

    let svcs = inv.list_services(None).await.unwrap();
    assert_eq!(svcs.len(), 1);
    assert_eq!(svcs[0].image, "nginx:2");
    assert!(matches!(svcs[0].state, ServiceState::Restarting));
}

#[tokio::test]
async fn queue_and_deliver_host_action() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = test_enroll(&inv, "h1").await;

    let action_id = inv.queue_action(host_id, HostActionKind::ForceUpdate { stack_name: None })
        .await.unwrap();

    let pending = inv.pending_actions(host_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, action_id);

    inv.mark_action_delivered(action_id, "ok").await.unwrap();
    let pending = inv.pending_actions(host_id).await.unwrap();
    assert!(pending.is_empty(), "delivered actions are not returned by pending_actions");
}

#[tokio::test]
async fn fleets_create_list_delete() {
    let inv = Inventory::open_in_memory().await.unwrap();

    inv.create_fleet("staging").await.unwrap();
    inv.create_fleet("prod").await.unwrap();

    let fleets = inv.list_fleets().await.unwrap();
    let names: std::collections::HashSet<_> = fleets.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains("default"));
    assert!(names.contains("staging"));
    assert!(names.contains("prod"));

    let deleted = inv.delete_fleet("staging").await.unwrap();
    assert!(deleted);

    // Can't delete a fleet that has hosts assigned
    let host_id = test_enroll(&inv, "h-prod").await;
    inv.set_host_fleet(host_id, "prod").await.unwrap();
    let err = inv.delete_fleet("prod").await;
    assert!(err.is_err(), "fleet with hosts should not be deletable");
}

#[tokio::test]
async fn settings_round_trip() {
    let inv = Inventory::open_in_memory().await.unwrap();

    inv.set_setting("notifier.telegram.enabled", &serde_json::json!(true))
        .await.unwrap();

    let v = inv.get_setting("notifier.telegram.enabled").await.unwrap();
    assert_eq!(v, Some(serde_json::json!(true)));

    let missing = inv.get_setting("nope").await.unwrap();
    assert!(missing.is_none());
}

async fn test_enroll(inv: &Inventory, hostname: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: format!("fp-{hostname}"),
        hostname: hostname.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
    }).await.unwrap()
}
```

- [ ] **Step 2: Run failing tests**

Run: `cargo test -p isengard-storage inventory --no-run`
Expected: FAIL — methods don't exist.

- [ ] **Step 3: Implement service methods**

```rust
use crate::service::{InsertService, Service, ServiceId, ServiceState};

pub async fn insert_service(&self, req: InsertService) -> Result<ServiceId> {
    sqlx::query(
        "INSERT INTO services (host_id, stack_id, name, image, state, last_seen_at)
         VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(host_id, name) DO UPDATE SET
             stack_id     = excluded.stack_id,
             image        = excluded.image,
             state        = excluded.state,
             last_seen_at = CURRENT_TIMESTAMP",
    )
    .bind(req.host_id.to_bytes().as_slice())
    .bind(req.stack_id.map(|s| s.0))
    .bind(&req.name)
    .bind(&req.image)
    .bind(req.state.as_str())
    .execute(&self.pool)
    .await?;

    let row = sqlx::query("SELECT id FROM services WHERE host_id = ? AND name = ?")
        .bind(req.host_id.to_bytes().as_slice())
        .bind(&req.name)
        .fetch_one(&self.pool)
        .await?;
    Ok(ServiceId(row.try_get("id")?))
}

pub async fn list_services(&self, stack_id: Option<StackId>) -> Result<Vec<Service>> {
    let rows = match stack_id {
        Some(s) => {
            sqlx::query("SELECT id, host_id, stack_id, name, image, state, last_seen_at
                         FROM services WHERE stack_id = ? ORDER BY name")
                .bind(s.0)
                .fetch_all(&self.pool)
                .await?
        }
        None => {
            sqlx::query("SELECT id, host_id, stack_id, name, image, state, last_seen_at
                         FROM services ORDER BY name")
                .fetch_all(&self.pool)
                .await?
        }
    };
    rows.into_iter().map(service_from_row).collect()
}

pub async fn get_service(&self, id: ServiceId) -> Result<Option<Service>> {
    let row = sqlx::query("SELECT id, host_id, stack_id, name, image, state, last_seen_at
                           FROM services WHERE id = ?")
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
    row.map(service_from_row).transpose()
}

pub async fn delete_service(&self, id: ServiceId) -> Result<bool> {
    let r = sqlx::query("DELETE FROM services WHERE id = ?")
        .bind(id.0)
        .execute(&self.pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

fn service_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Service> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    Ok(Service {
        id: ServiceId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        stack_id: row.try_get::<Option<i64>, _>("stack_id")?.map(StackId),
        name: row.try_get("name")?,
        image: row.try_get("image")?,
        state: ServiceState::from_str(&row.try_get::<String, _>("state")?),
        last_seen_at: row.try_get("last_seen_at")?,
    })
}
```

- [ ] **Step 4: Implement host_action methods**

```rust
use crate::host_action::{HostAction, HostActionId, HostActionKind};

pub async fn queue_action(&self, host_id: HostId, kind: HostActionKind) -> Result<HostActionId> {
    let kind_str = kind.kind_str();
    let payload = kind.payload_json();
    let r = sqlx::query("INSERT INTO host_actions (host_id, kind, payload_json) VALUES (?, ?, ?)")
        .bind(host_id.to_bytes().as_slice())
        .bind(kind_str)
        .bind(payload)
        .execute(&self.pool)
        .await?;
    Ok(HostActionId(r.last_insert_rowid()))
}

pub async fn pending_actions(&self, host_id: HostId) -> Result<Vec<HostAction>> {
    let rows = sqlx::query(
        "SELECT id, host_id, kind, payload_json, created_at, delivered_at, result
         FROM host_actions WHERE host_id = ? AND delivered_at IS NULL ORDER BY id",
    )
    .bind(host_id.to_bytes().as_slice())
    .fetch_all(&self.pool)
    .await?;

    rows.into_iter().map(host_action_from_row).collect()
}

pub async fn mark_action_delivered(&self, id: HostActionId, result: &str) -> Result<()> {
    sqlx::query(
        "UPDATE host_actions SET delivered_at = CURRENT_TIMESTAMP, result = ? WHERE id = ?",
    )
    .bind(result)
    .bind(id.0)
    .execute(&self.pool)
    .await?;
    Ok(())
}

fn host_action_from_row(row: sqlx::sqlite::SqliteRow) -> Result<HostAction> {
    use sqlx::Row;
    let host_bytes: Vec<u8> = row.try_get("host_id")?;
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&host_bytes);
    let payload: String = row.try_get("payload_json")?;
    let kind: HostActionKind = serde_json::from_str(&payload)
        .map_err(|e| Error::InvalidData(format!("bad host_action payload: {e}")))?;
    Ok(HostAction {
        id: HostActionId(row.try_get("id")?),
        host_id: HostId::from_bytes(arr),
        kind,
        created_at: row.try_get("created_at")?,
        delivered_at: row.try_get("delivered_at")?,
        result: row.try_get("result")?,
    })
}
```

- [ ] **Step 5: Implement fleet methods**

```rust
use crate::fleet::Fleet;

pub async fn create_fleet(&self, name: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO fleets (name) VALUES (?)")
        .bind(name)
        .execute(&self.pool)
        .await?;
    Ok(())
}

pub async fn list_fleets(&self) -> Result<Vec<Fleet>> {
    let rows = sqlx::query("SELECT name, created_at FROM fleets ORDER BY name")
        .fetch_all(&self.pool)
        .await?;
    rows.into_iter()
        .map(|r| {
            use sqlx::Row;
            Ok(Fleet {
                name: r.try_get("name")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn delete_fleet(&self, name: &str) -> Result<bool> {
    // Refuse if any hosts are assigned to this fleet.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hosts WHERE fleet = ?")
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
    if count > 0 {
        return Err(Error::Conflict(format!(
            "fleet '{name}' has {count} hosts assigned"
        )));
    }
    let r = sqlx::query("DELETE FROM fleets WHERE name = ?")
        .bind(name)
        .execute(&self.pool)
        .await?;
    Ok(r.rows_affected() > 0)
}
```

(Add a `Conflict(String)` variant to `Error` if it doesn't exist.)

- [ ] **Step 6: Implement settings methods**

```rust
pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
    let json = serde_json::to_string(value).map_err(|e| Error::InvalidData(e.to_string()))?;
    sqlx::query(
        "INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = CURRENT_TIMESTAMP",
    )
    .bind(key)
    .bind(&json)
    .execute(&self.pool)
    .await?;
    Ok(())
}

pub async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
    let row = sqlx::query("SELECT value_json FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
    match row {
        Some(r) => {
            use sqlx::Row;
            let json: String = r.try_get("value_json")?;
            Ok(Some(serde_json::from_str(&json).map_err(|e| Error::InvalidData(e.to_string()))?))
        }
        None => Ok(None),
    }
}

pub async fn list_settings(&self) -> Result<Vec<Setting>> {
    let rows = sqlx::query("SELECT key, value_json, updated_at FROM settings ORDER BY key")
        .fetch_all(&self.pool)
        .await?;
    rows.into_iter()
        .map(|r| {
            use sqlx::Row;
            let json: String = r.try_get("value_json")?;
            let value = serde_json::from_str(&json).map_err(|e| Error::InvalidData(e.to_string()))?;
            Ok(Setting {
                key: r.try_get("key")?,
                value,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p isengard-storage`
Expected: PASS — all four new tests + existing.

- [ ] **Step 8: Commit**

```bash
git add crates/isengard-storage/src/inventory.rs crates/isengard-storage/src/error.rs
git commit -m "feat(storage): inventory service + host_action + fleet + setting methods"
```

---

## Task 3: Proto extensions for ServiceInfo and PendingAction

**Files:**
- Modify: `crates/isengard-proto/proto/sync.proto`

- [ ] **Step 1: Add new messages**

Append to `sync.proto`:

```proto
message ServiceInfo {
  string name = 1;
  string image = 2;
  // One of: "running", "stopped", "restarting", "unknown".
  string state = 3;
  // Stack name this service belongs to (if any). Same value the agent reported
  // in the matching StackInfo.
  optional string stack = 4;
}

message PendingAction {
  uint64 id = 1;
  // One of: "force_update", "decommission".
  string kind = 2;
  // JSON-encoded HostActionKind payload.
  string payload_json = 3;
}
```

In `Heartbeat` (or `InventorySnapshot`), add the next free tag:

```proto
  repeated ServiceInfo services = <N>;
```

In `SyncResponse` (the controller's reply to a heartbeat), add:

```proto
  repeated PendingAction pending_actions = <N>;
```

- [ ] **Step 2: Build to regenerate bindings**

Run: `cargo build -p isengard-proto`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/isengard-proto/proto/sync.proto
git commit -m "proto: + ServiceInfo + PendingAction; Heartbeat carries services, SyncResponse carries pending"
```

---

## Task 4: Agent — emit ServiceInfo + execute pending actions

**Files:**
- Modify: `crates/isengard-agent/src/inventory_snapshot.rs`
- Modify: `crates/isengard-agent/src/sync.rs` (or wherever it processes the controller's reply)

- [ ] **Step 1: Add a unit test for derive_services**

```rust
#[test]
fn derives_service_info_with_state_and_stack_link() {
    let containers = vec![
        ContainerSnapshot {
            name: "web".into(),
            image: "nginx:1.25-alpine".into(),
            state: "running".into(),
            labels: hashmap! {
                "com.docker.compose.project".to_string() => "blog".to_string(),
            },
        },
        ContainerSnapshot {
            name: "homer".into(),
            image: "b4bz/homer:latest".into(),
            state: "stopped".into(),
            labels: HashMap::new(),
        },
    ];

    let services = derive_services(&containers);
    assert_eq!(services.len(), 2);

    let web = services.iter().find(|s| s.name == "web").unwrap();
    assert_eq!(web.image, "nginx:1.25-alpine");
    assert_eq!(web.state, "running");
    assert_eq!(web.stack.as_deref(), Some("blog"));

    let homer = services.iter().find(|s| s.name == "homer").unwrap();
    assert_eq!(homer.state, "stopped");
    assert_eq!(homer.stack.as_deref(), Some("homer")); // inferred = own name
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test -p isengard-agent derives_service_info --no-run`
Expected: FAIL.

- [ ] **Step 3: Implement derive_services**

```rust
use isengard_proto::sync::ServiceInfo;

pub fn derive_services(containers: &[ContainerSnapshot]) -> Vec<ServiceInfo> {
    containers.iter().map(|c| {
        let stack = c.labels.get("isengard.stack")
            .or_else(|| c.labels.get("com.docker.compose.project"))
            .cloned()
            .unwrap_or_else(|| c.name.clone());

        ServiceInfo {
            name: c.name.clone(),
            image: c.image.clone(),
            state: c.state.clone(),
            stack: Some(stack),
        }
    }).collect()
}
```

Wire into the heartbeat builder alongside `derive_stacks`:

```rust
let services = derive_services(&container_snapshots);
let stacks = derive_stacks(&container_snapshots);

let heartbeat = Heartbeat {
    // ...
    stacks,
    services,
};
```

- [ ] **Step 4: Process pending_actions from controller reply**

```rust
// In sync.rs, after receiving SyncResponse:

for action in response.pending_actions {
    let result = match action.kind.as_str() {
        "force_update" => {
            // Parse payload: { stack_name?: String }
            #[derive(serde::Deserialize, Default)]
            struct Payload { stack_name: Option<String> }
            let p: Payload = serde_json::from_str(&action.payload_json).unwrap_or_default();

            match self.updater.run_now(p.stack_name.as_deref()).await {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            }
        }
        "decommission" => {
            // Mark agent for shutdown after this cycle.
            self.shutdown.notify_one();
            "ok".to_string()
        }
        unknown => format!("unknown kind: {unknown}"),
    };

    // Emit a dispatch event so the dashboard sees it.
    self.emitter.emit(Event {
        kind: "dashboard.action_dispatched".into(),
        summary: format!("action #{} ({}) → {}", action.id, action.kind, result),
        metadata: serde_json::json!({
            "action_id": action.id,
            "kind": action.kind,
            "result": result,
        }),
        ..Event::default()
    }).await?;
}
```

(Adjust to whatever `Updater::run_now` signature actually exists; if it accepts a service name instead, plumb the right argument. The point is: each pending action is dispatched, then an event is emitted with the result.)

- [ ] **Step 5: Run tests**

Run: `cargo test -p isengard-agent`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-agent/src/inventory_snapshot.rs \
        crates/isengard-agent/src/sync.rs
git commit -m "feat(agent): emit ServiceInfo in heartbeat + execute pending actions from controller"
```

---

## Task 5: Controller — persist services + attach pending_actions

**Files:**
- Modify: `crates/isengard-controller/src/sync.rs`

- [ ] **Step 1: Write failing integration test**

```rust
#[tokio::test]
async fn services_persisted_and_pending_actions_attached() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = inv.enroll_host(EnrollHost {
        fingerprint: "fp".into(), hostname: "h".into(),
        os: "linux".into(), arch: "x86_64".into(),
        agent_version: "0.1.0".into(), docker_version: "27.0".into(),
    }).await.unwrap();

    // Queue a force-update before the next heartbeat
    let action_id = inv.queue_action(host_id, HostActionKind::ForceUpdate { stack_name: None })
        .await.unwrap();

    // Build a heartbeat with services
    let services = vec![
        ServiceInfo { name: "web".into(), image: "nginx".into(), state: "running".into(), stack: Some("blog".into()) },
    ];

    // Process
    let response = process_heartbeat_v2(&inv, host_id, &services, &[]).await.unwrap();

    // Services persisted
    let stored = inv.list_services(None).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].name, "web");

    // Action attached + marked delivered
    assert_eq!(response.pending_actions.len(), 1);
    assert_eq!(response.pending_actions[0].id, action_id.0 as u64);

    let still_pending = inv.pending_actions(host_id).await.unwrap();
    assert!(still_pending.is_empty());
}
```

- [ ] **Step 2: Implement service persistence + action attachment**

In `sync.rs`, extend the heartbeat handler:

```rust
pub async fn process_services(
    inv: &Inventory,
    host_id: HostId,
    services: &[ProtoServiceInfo],
) -> Result<()> {
    let mut current_names = HashSet::new();
    for s in services {
        // Resolve stack_id by looking up stack_name on this host.
        let stack_id = match s.stack.as_deref() {
            Some(name) => {
                inv.list_stacks(Some(host_id)).await?
                    .into_iter()
                    .find(|st| st.name == name)
                    .map(|st| st.id)
            }
            None => None,
        };

        inv.insert_service(InsertService {
            host_id,
            stack_id,
            name: s.name.clone(),
            image: s.image.clone(),
            state: ServiceState::from_str(&s.state),
        }).await?;
        current_names.insert(s.name.clone());
    }

    // Prune
    let existing = inv.list_services(None).await?
        .into_iter().filter(|sv| sv.host_id == host_id).collect::<Vec<_>>();
    for sv in existing {
        if !current_names.contains(&sv.name) {
            inv.delete_service(sv.id).await?;
        }
    }

    Ok(())
}

pub async fn collect_pending_actions(
    inv: &Inventory,
    host_id: HostId,
) -> Result<Vec<ProtoPendingAction>> {
    let pending = inv.pending_actions(host_id).await?;
    let mut out = Vec::with_capacity(pending.len());
    for action in pending {
        out.push(ProtoPendingAction {
            id: action.id.0 as u64,
            kind: action.kind.kind_str().to_string(),
            payload_json: action.kind.payload_json(),
        });
        // Mark as delivered immediately. The agent's response (or next heartbeat)
        // can update the result; we don't block the reply on it.
        inv.mark_action_delivered(action.id, "dispatched").await?;
    }
    Ok(out)
}
```

Wire both into the existing heartbeat handler so it calls `process_services` after `process_heartbeat_stacks`, and includes `collect_pending_actions(...).await?` in the reply.

- [ ] **Step 3: Run tests**

Run: `cargo test -p isengard-controller`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/isengard-controller/src/sync.rs
git commit -m "feat(controller): persist services + dispatch pending actions in heartbeat reply"
```

---

## Task 6: Dashboard API — real enroll, force-update, fleet CRUD, settings

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/dto.rs`
- Modify: `crates/isengard-plugins/dashboard/src/api.rs`

- [ ] **Step 1: Add DTOs**

```rust
#[derive(Debug, Clone, Serialize)]
pub struct FleetDto {
    pub name: String,
    pub host_count: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateFleetBody {
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentDto {
    pub agent_id: String,
    pub enrollment_token: String,
    /// Single-line shell command the user pastes onto the new host.
    pub install_command: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollHostBody {
    pub fleet: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsDto {
    /// Flat key/value snapshot. Each value is a JSON value (bool, string, number, object).
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PatchSettingsBody {
    pub values: serde_json::Map<String, serde_json::Value>,
}
```

- [ ] **Step 2: Replace force-update stubs**

```rust
pub async fn force_update_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let host_id = parse_host_id(&id)?;
    handles.inventory.queue_action(host_id, HostActionKind::ForceUpdate { stack_name: None })
        .await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn force_update_stack(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let stack = handles.inventory.get_stack(StackId(id)).await?
        .ok_or(ApiError::NotFound)?;
    handles.inventory.queue_action(
        stack.host_id,
        HostActionKind::ForceUpdate { stack_name: Some(stack.name) },
    ).await?;
    Ok(StatusCode::ACCEPTED)
}
```

- [ ] **Step 3: Implement enroll handler**

```rust
pub async fn enroll_host(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<EnrollHostBody>,
) -> Result<Json<EnrollmentDto>, ApiError> {
    // Generate a one-time enrollment token. v1 uses a random ULID; the agent
    // exchanges this for an auth token on first contact.
    let token = ulid::Ulid::new().to_string();
    let agent_id = ulid::Ulid::new().to_string();

    // Persist token (for v1 we keep it in settings; v1.x adds a dedicated
    // enrollment_tokens table with TTL).
    handles.inventory.set_setting(
        &format!("enrollment.token.{token}"),
        &serde_json::json!({
            "agent_id": agent_id,
            "fleet": body.fleet.clone().unwrap_or_else(|| "default".into()),
            "hostname": body.hostname.clone(),
        }),
    ).await?;

    // Build the install command. The controller URL comes from the app config.
    let controller_url = handles.config.public_url.clone();
    let install_command = format!(
        "curl -fsSL {controller_url}/install.sh | sh -s -- --token {token}",
    );

    Ok(Json(EnrollmentDto {
        agent_id,
        enrollment_token: token,
        install_command,
    }))
}
```

(`handles.config` may not exist yet — extend `ControllerHandles` to carry an `Arc<DashboardConfig>` with at least `public_url: String` defaulting to `http://127.0.0.1:9418`.)

- [ ] **Step 4: Implement fleet CRUD**

```rust
pub async fn list_fleets(
    State(handles): State<Arc<ControllerHandles>>,
) -> Result<Json<Vec<FleetDto>>, ApiError> {
    let fleets = handles.inventory.list_fleets().await?;
    let hosts = handles.inventory.list_hosts().await?;

    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for h in hosts {
        *counts.entry(h.fleet).or_default() += 1;
    }

    let dtos = fleets.into_iter().map(|f| FleetDto {
        name: f.name.clone(),
        host_count: counts.get(&f.name).copied().unwrap_or(0),
        created_at: f.created_at,
    }).collect();

    Ok(Json(dtos))
}

pub async fn create_fleet(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateFleetBody>,
) -> Result<StatusCode, ApiError> {
    if body.name.is_empty() || body.name.len() > 32 {
        return Err(ApiError::BadRequest("fleet name must be 1-32 chars".into()));
    }
    handles.inventory.create_fleet(&body.name).await?;
    Ok(StatusCode::CREATED)
}

pub async fn delete_fleet(
    State(handles): State<Arc<ControllerHandles>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if name == "default" {
        return Err(ApiError::BadRequest("cannot delete the default fleet".into()));
    }
    let deleted = handles.inventory.delete_fleet(&name).await?;
    if deleted { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::NotFound) }
}
```

- [ ] **Step 5: Implement settings handlers**

```rust
pub async fn get_settings(
    State(handles): State<Arc<ControllerHandles>>,
) -> Result<Json<SettingsDto>, ApiError> {
    let all = handles.inventory.list_settings().await?;
    let mut values = serde_json::Map::new();
    for s in all {
        // Hide enrollment.token.* — they're one-time secrets.
        if s.key.starts_with("enrollment.token.") { continue; }
        values.insert(s.key, s.value);
    }
    Ok(Json(SettingsDto { values }))
}

pub async fn patch_settings(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<PatchSettingsBody>,
) -> Result<StatusCode, ApiError> {
    for (key, value) in body.values {
        if key.starts_with("enrollment.token.") {
            return Err(ApiError::BadRequest("cannot set enrollment tokens via settings".into()));
        }
        handles.inventory.set_setting(&key, &value).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
```

- [ ] **Step 6: Wire all routes**

```rust
.route("/api/v1/hosts/enroll",                       post(enroll_host))
.route("/api/v1/hosts/:id/actions/force-update",     post(force_update_host))
.route("/api/v1/stacks/:id/actions/force-update",    post(force_update_stack))
.route("/api/v1/fleets",                             get(list_fleets).post(create_fleet))
.route("/api/v1/fleets/:name",                       delete(delete_fleet))
.route("/api/v1/settings",                           get(get_settings).patch(patch_settings))
```

- [ ] **Step 7: Add tests for the new handlers**

```rust
#[tokio::test]
async fn force_update_host_queues_action() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host_id = test_enroll(&inv).await;

    let app = build_router(test_handles(inv.clone()));
    let resp = app.oneshot(Request::builder()
        .method("POST")
        .uri(format!("/api/v1/hosts/{host_id}/actions/force-update"))
        .body(Body::empty()).unwrap())
        .await.unwrap();

    assert_eq!(resp.status(), 202);
    let pending = inv.pending_actions(host_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].kind, HostActionKind::ForceUpdate { stack_name: None }));
}

#[tokio::test]
async fn create_and_delete_fleet_with_host_check() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let app = build_router(test_handles(inv.clone()));

    let resp = app.clone().oneshot(Request::builder()
        .method("POST").uri("/api/v1/fleets")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"prod"}"#)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), 201);

    let resp = app.clone().oneshot(Request::builder()
        .method("DELETE").uri("/api/v1/fleets/prod")
        .body(Body::empty()).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn enroll_host_returns_install_command() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let app = build_router(test_handles(inv));

    let resp = app.oneshot(Request::builder()
        .method("POST").uri("/api/v1/hosts/enroll")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"fleet":"staging"}"#)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let dto: EnrollmentDto = serde_json::from_slice(&body).unwrap();
    assert!(dto.install_command.starts_with("curl"));
    assert!(dto.install_command.contains(&dto.enrollment_token));
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p isengard-dashboard`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/isengard-plugins/dashboard/src/dto.rs \
        crates/isengard-plugins/dashboard/src/api.rs \
        crates/isengard-plugins/dashboard/src/lib.rs
git commit -m "feat(dashboard): real enroll + force-update + fleet CRUD + settings handlers"
```

---

## Task 7: Dashboard WS — read-only log streaming

**Files:**
- Modify: `crates/isengard-plugins/dashboard/src/ws.rs`
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs` (route registration)

- [ ] **Step 1: Establish controller→agent log streaming primitive**

The controller must be able to ask an agent: "stream stdout/stderr of container X to me." For v1 we add a simple gRPC server-streaming RPC to the existing `AgentControl` service:

In `crates/isengard-proto/proto/control.proto` (or wherever the agent-bound RPCs live), add:

```proto
message StreamLogsRequest {
  string container_name = 1;
  optional uint32 tail = 2; // last N lines on connect; default 200
}

message LogLine {
  // "stdout" or "stderr"
  string stream = 1;
  string text = 2;
  // Unix nanos
  int64 ts = 3;
}

service AgentControl {
  // ... existing RPCs ...
  rpc StreamLogs(StreamLogsRequest) returns (stream LogLine);
}
```

Implement on the agent side (`crates/isengard-agent/src/grpc.rs` or similar):

```rust
async fn stream_logs(
    &self,
    req: Request<StreamLogsRequest>,
) -> Result<Response<Self::StreamLogsStream>, Status> {
    let req = req.into_inner();
    let tail = req.tail.unwrap_or(200);

    // Spawn `docker logs --tail=N -f <container>` and forward lines.
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        let mut child = tokio::process::Command::new("docker")
            .args(["logs", "--tail", &tail.to_string(), "-f", &req.container_name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Status::internal(format!("docker logs spawn: {e}")))?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let tx2 = tx.clone();

        tokio::spawn(forward_lines(stdout, "stdout".into(), tx.clone()));
        tokio::spawn(forward_lines(stderr, "stderr".into(), tx2));

        let _ = child.wait().await;
        Ok::<_, Status>(())
    });

    Ok(Response::new(
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)) as Self::StreamLogsStream,
    ))
}

async fn forward_lines<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    stream: String,
    tx: tokio::sync::mpsc::Sender<Result<LogLine, Status>>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        let line = LogLine {
            stream: stream.clone(),
            text,
            ts: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        };
        if tx.send(Ok(line)).await.is_err() { break; }
    }
}
```

- [ ] **Step 2: Add the WS handler in dashboard**

```rust
// crates/isengard-plugins/dashboard/src/ws.rs

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use isengard_storage::ServiceId;

pub async fn handle_logs(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| log_session(socket, handles, ServiceId(id)))
}

async fn log_session(
    mut socket: WebSocket,
    handles: Arc<ControllerHandles>,
    service_id: ServiceId,
) {
    // Resolve service → host
    let svc = match handles.inventory.get_service(service_id).await {
        Ok(Some(s)) => s,
        _ => {
            let _ = socket.send(Message::Text(
                r#"{"type":"error","error":"service not found"}"#.into(),
            )).await;
            return;
        }
    };

    // Open RPC stream to agent. ControllerHandles needs a way to reach the
    // agent — extend with `agent_clients: Arc<AgentClientPool>` exposing
    // `pool.stream_logs(host_id, container_name) -> impl Stream<LogLine>`.
    let mut log_stream = match handles.agent_clients.stream_logs(svc.host_id, &svc.name).await {
        Ok(s) => s,
        Err(e) => {
            let _ = socket.send(Message::Text(
                serde_json::json!({"type":"error","error": e.to_string()}).to_string(),
            )).await;
            return;
        }
    };

    use futures::StreamExt;
    loop {
        tokio::select! {
            // Forward agent log lines to the browser
            line = log_stream.next() => {
                match line {
                    Some(Ok(line)) => {
                        let frame = serde_json::json!({
                            "type": "line",
                            "stream": line.stream,
                            "text": line.text,
                            "ts": line.ts,
                        }).to_string();
                        if socket.send(Message::Text(frame)).await.is_err() { break; }
                    }
                    Some(Err(e)) => {
                        let _ = socket.send(Message::Text(
                            serde_json::json!({"type":"error","error": e.to_string()}).to_string(),
                        )).await;
                        break;
                    }
                    None => break, // upstream closed
                }
            }
            // Browser sent a frame (close request)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Text(t))) => {
                        if t.contains("\"close\"") { break; }
                    }
                    _ => {}
                }
            }
        }
    }
}
```

(For the unit-test path, `agent_clients` can be a trait object that returns an in-process channel; the integration test uses that to verify framing.)

- [ ] **Step 3: Register the route**

In `lib.rs`, add to the router:

```rust
.route("/api/v1/services/:id/logs", get(crate::ws::handle_logs))
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p isengard-dashboard`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-proto/proto/control.proto \
        crates/isengard-agent/src/grpc.rs \
        crates/isengard-plugins/dashboard/src/ws.rs \
        crates/isengard-plugins/dashboard/src/lib.rs
git commit -m "feat(dashboard): + read-only log WS streaming via agent gRPC StreamLogs"
```

---

## Task 8: Frontend — useLogStream + settings store + fleets store extension

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/composables/useLogStream.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/settings.ts`
- Modify: `crates/isengard-plugins/dashboard/web/stores/fleets.ts`

- [ ] **Step 1: Write useLogStream.ts**

```typescript
import { ref, type Ref } from 'vue'

export interface LogLine {
  stream: 'stdout' | 'stderr'
  text: string
  ts: number
}

export function useLogStream(serviceId: Ref<string> | string) {
  const lines = ref<LogLine[]>([])
  const connected = ref(false)
  let ws: WebSocket | null = null

  function connect() {
    const id = typeof serviceId === 'string' ? serviceId : serviceId.value
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    ws = new WebSocket(`${proto}//${window.location.host}/api/v1/services/${id}/logs`)
    ws.onopen = () => { connected.value = true }
    ws.onclose = () => { connected.value = false }
    ws.onmessage = (ev) => {
      try {
        const frame = JSON.parse(ev.data)
        if (frame.type === 'line') {
          lines.value.push({ stream: frame.stream, text: frame.text, ts: frame.ts })
        }
      } catch {}
    }
  }

  function disconnect() {
    if (ws) {
      try { ws.send(JSON.stringify({ type: 'close' })) } catch {}
      ws.close()
      ws = null
    }
    connected.value = false
  }

  return { lines, connected, connect, disconnect }
}
```

- [ ] **Step 2: Write settings store**

```typescript
import { defineStore } from 'pinia'
import { useApi } from '~/composables/useApi'

export const useSettingsStore = defineStore('settings', {
  state: () => ({
    values: {} as Record<string, unknown>,
    loaded: false,
  }),

  actions: {
    async fetch() {
      const api = useApi()
      const data = await api.get<{ values: Record<string, unknown> }>('/api/v1/settings')
      this.values = data.values
      this.loaded = true
    },

    async patch(updates: Record<string, unknown>) {
      const api = useApi()
      await api.patch('/api/v1/settings', { values: updates })
      Object.assign(this.values, updates)
    },
  },
})
```

- [ ] **Step 3: Extend fleets store with create/delete**

```typescript
// crates/isengard-plugins/dashboard/web/stores/fleets.ts
import { defineStore } from 'pinia'
import { useApi } from '~/composables/useApi'

export interface Fleet {
  name: string
  host_count: number
  created_at: string
}

export const useFleetsStore = defineStore('fleets', {
  state: () => ({
    items: [] as Fleet[],
    loaded: false,
  }),

  actions: {
    async fetchAll() {
      const api = useApi()
      this.items = await api.get<Fleet[]>('/api/v1/fleets')
      this.loaded = true
    },

    async create(name: string) {
      const api = useApi()
      await api.post('/api/v1/fleets', { name })
      await this.fetchAll()
    },

    async remove(name: string) {
      const api = useApi()
      await api.delete(`/api/v1/fleets/${name}`)
      this.items = this.items.filter((f) => f.name !== name)
    },
  },
})
```

- [ ] **Step 4: Build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/composables/useLogStream.ts \
        crates/isengard-plugins/dashboard/web/stores/settings.ts \
        crates/isengard-plugins/dashboard/web/stores/fleets.ts
git commit -m "feat(dashboard-web): useLogStream + settings store + fleets create/delete"
```

---

## Task 9: Events page — full filterable journal

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/EventFilterChip.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/events/index.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/events/[id].vue`

- [ ] **Step 1: Write EventFilterChip.vue**

```vue
<script setup lang="ts">
interface Props {
  label: string
  active?: boolean
  count?: number
}

defineProps<Props>()
defineEmits<{ toggle: [] }>()
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-xs border transition-colors"
    :class="active
      ? 'border-iso-success bg-iso-success/10 text-iso-success'
      : 'border-iso-border text-iso-text-muted hover:text-iso-text-base'"
    @click="$emit('toggle')"
  >
    {{ label }}
    <span v-if="count !== undefined" class="font-mono text-[10px] opacity-70">{{ count }}</span>
  </button>
</template>
```

- [ ] **Step 2: Write pages/events/index.vue**

```vue
<script setup lang="ts">
import { useEventsStore } from '~/stores/events'
import { useHostsStore } from '~/stores/hosts'
import { useUiStore } from '~/stores/ui'

const eventsStore = useEventsStore()
const hostsStore  = useHostsStore()
const uiStore     = useUiStore()

await Promise.all([
  eventsStore.fetchRecent({ limit: 500 }),
  hostsStore.fetchAll(),
])

const KINDS = ['UPDATED', 'FAILED', 'CHECKED', 'PULLING', 'DISCONNECT'] as const
const activeKinds = ref(new Set<string>(KINDS))
const activeHostId = ref<string | null>(null)

function toggleKind(kind: string) {
  if (activeKinds.value.has(kind)) activeKinds.value.delete(kind)
  else activeKinds.value.add(kind)
}

const counts = computed(() => {
  const c: Record<string, number> = {}
  for (const k of KINDS) c[k] = 0
  for (const e of eventsStore.items) {
    const key = e.kind.toUpperCase()
    if (key in c) c[key]++
  }
  return c
})

const filtered = computed(() => {
  return eventsStore.items.filter((e) => {
    if (!activeKinds.value.has(e.kind.toUpperCase())) return false
    if (activeHostId.value && e.host_id !== activeHostId.value) return false
    return true
  })
})

const router = useRouter()
function openEvent(e: { id: number }) {
  router.push(`/events/${e.id}`)
}
</script>

<template>
  <div class="flex flex-col h-full">
    <div class="flex items-center gap-2 px-4 py-3 border-b border-iso-border flex-wrap">
      <span class="text-xs uppercase tracking-wider text-iso-text-faint mr-2">Filter</span>
      <EventFilterChip
        v-for="k in KINDS"
        :key="k"
        :label="k"
        :count="counts[k]"
        :active="activeKinds.has(k)"
        @toggle="toggleKind(k)"
      />
      <select
        v-model="activeHostId"
        class="ml-2 bg-iso-bg-elevated border border-iso-border rounded px-2 py-1 text-xs"
      >
        <option :value="null">All hosts</option>
        <option v-for="h in hostsStore.items" :key="h.id" :value="h.id">{{ h.hostname }}</option>
      </select>
      <span class="ml-auto text-xs text-iso-text-muted">
        {{ filtered.length }} / {{ eventsStore.items.length }} events
      </span>
    </div>

    <div class="flex-1 overflow-y-auto">
      <EventRow
        v-for="e in filtered"
        :key="e.id"
        :event="e"
        :selected="false"
        @click="openEvent(e)"
      />
    </div>
  </div>
</template>
```

- [ ] **Step 3: Write pages/events/[id].vue**

```vue
<script setup lang="ts">
import { useEventsStore } from '~/stores/events'

const route = useRoute()
const id = computed(() => Number(route.params.id))

const eventsStore = useEventsStore()
await eventsStore.fetchOne(id.value)

const event = computed(() => eventsStore.items.find((e) => e.id === id.value))
</script>

<template>
  <div v-if="event" class="p-6 max-w-3xl">
    <NuxtLink to="/events" class="text-xs text-iso-text-muted hover:text-iso-text-base">
      ← Events
    </NuxtLink>
    <h1 class="font-mono text-lg mt-1">
      <span class="text-iso-success">{{ event.kind }}</span>
      {{ event.summary }}
    </h1>
    <div class="text-sm text-iso-text-muted mt-1">
      {{ event.occurred_at }} · received {{ event.received_at }}
    </div>

    <section class="mt-6">
      <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-2">Metadata</h2>
      <pre class="text-xs font-mono bg-iso-bg-elevated rounded p-3 overflow-x-auto">{{ JSON.stringify(event.metadata, null, 2) }}</pre>
    </section>
  </div>
  <div v-else class="p-6 text-iso-text-muted">Event not found.</div>
</template>
```

(Add a `fetchOne(id)` action to the events store if it doesn't exist — small REST GET to `/api/v1/events/:id` that merges into items.)

- [ ] **Step 4: Build + commit**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

```bash
git add crates/isengard-plugins/dashboard/web/components/EventFilterChip.vue \
        crates/isengard-plugins/dashboard/web/pages/events/index.vue \
        crates/isengard-plugins/dashboard/web/pages/events/[id].vue \
        crates/isengard-plugins/dashboard/web/stores/events.ts
git commit -m "feat(dashboard-web): /events filterable journal + /events/:id detail"
```

---

## Task 10: Settings page — Fleets, Notifiers, Enrollment

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/SettingsSection.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/FleetsSettings.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/NotifierSettings.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/EnrollmentSettings.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`

- [ ] **Step 1: Write SettingsSection.vue**

```vue
<script setup lang="ts">
interface Props {
  title: string
  description?: string
}
defineProps<Props>()
</script>

<template>
  <section class="border-b border-iso-border py-6">
    <header class="mb-4">
      <h2 class="font-mono text-base">{{ title }}</h2>
      <p v-if="description" class="text-sm text-iso-text-muted mt-1">{{ description }}</p>
    </header>
    <slot />
  </section>
</template>
```

- [ ] **Step 2: Write FleetsSettings.vue**

```vue
<script setup lang="ts">
import { useFleetsStore } from '~/stores/fleets'

const fleetsStore = useFleetsStore()
await fleetsStore.fetchAll()

const newFleetName = ref('')
const error = ref('')

async function create() {
  error.value = ''
  try {
    await fleetsStore.create(newFleetName.value.trim())
    newFleetName.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

async function remove(name: string) {
  error.value = ''
  try {
    await fleetsStore.remove(name)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
</script>

<template>
  <SettingsSection title="Fleets" description="User-defined tags for grouping hosts.">
    <div class="space-y-2 mb-4">
      <div
        v-for="f in fleetsStore.items"
        :key="f.name"
        class="flex items-center justify-between px-3 py-2 rounded border border-iso-border bg-iso-bg-elevated"
      >
        <div class="flex items-center gap-3">
          <span class="font-mono text-sm">{{ f.name }}</span>
          <span class="text-xs text-iso-text-muted">{{ f.host_count }} hosts</span>
        </div>
        <button
          v-if="f.name !== 'default'"
          class="text-xs text-iso-error hover:underline"
          :disabled="f.host_count > 0"
          :title="f.host_count > 0 ? 'Cannot delete fleet with hosts' : ''"
          @click="remove(f.name)"
        >
          Delete
        </button>
      </div>
    </div>

    <form class="flex items-center gap-2" @submit.prevent="create">
      <input
        v-model="newFleetName"
        type="text"
        placeholder="new-fleet-name"
        class="bg-iso-bg-elevated border border-iso-border rounded px-3 py-1.5 text-sm font-mono w-64"
      />
      <button
        type="submit"
        class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-success hover:text-iso-success"
        :disabled="!newFleetName.trim()"
      >
        + New fleet
      </button>
    </form>

    <p v-if="error" class="text-xs text-iso-error mt-2">{{ error }}</p>
  </SettingsSection>
</template>
```

- [ ] **Step 3: Write NotifierSettings.vue**

```vue
<script setup lang="ts">
import { useSettingsStore } from '~/stores/settings'

const settings = useSettingsStore()
if (!settings.loaded) await settings.fetch()

const telegramEnabled = computed({
  get: () => Boolean(settings.values['notifier.telegram.enabled']),
  set: (v: boolean) => settings.patch({ 'notifier.telegram.enabled': v }),
})

const discordEnabled = computed({
  get: () => Boolean(settings.values['notifier.discord.enabled']),
  set: (v: boolean) => settings.patch({ 'notifier.discord.enabled': v }),
})
</script>

<template>
  <SettingsSection
    title="Notifiers"
    description="Send events to external chat platforms. Configure secrets in the controller config file."
  >
    <div class="space-y-3">
      <label class="flex items-center justify-between px-3 py-2 rounded border border-iso-border bg-iso-bg-elevated cursor-pointer">
        <div>
          <div class="font-mono text-sm">Telegram</div>
          <div class="text-xs text-iso-text-muted">Set TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID env vars on the controller.</div>
        </div>
        <input v-model="telegramEnabled" type="checkbox" class="w-4 h-4" />
      </label>

      <label class="flex items-center justify-between px-3 py-2 rounded border border-iso-border bg-iso-bg-elevated cursor-pointer">
        <div>
          <div class="font-mono text-sm">Discord</div>
          <div class="text-xs text-iso-text-muted">Set DISCORD_WEBHOOK_URL on the controller.</div>
        </div>
        <input v-model="discordEnabled" type="checkbox" class="w-4 h-4" />
      </label>
    </div>
  </SettingsSection>
</template>
```

- [ ] **Step 4: Write EnrollmentSettings.vue**

```vue
<script setup lang="ts">
const showModal = ref(false)
</script>

<template>
  <SettingsSection
    title="Agent enrollment"
    description="Add a new host by running a generated install command on it."
  >
    <button
      class="px-4 py-2 rounded border border-iso-border hover:border-iso-success hover:text-iso-success"
      @click="showModal = true"
    >
      + Generate install command
    </button>

    <AddHostModal v-if="showModal" @close="showModal = false" />
  </SettingsSection>
</template>
```

- [ ] **Step 5: Write pages/settings/index.vue**

```vue
<template>
  <div class="max-w-3xl mx-auto p-6">
    <h1 class="font-mono text-xl mb-1">Settings</h1>
    <p class="text-sm text-iso-text-muted mb-6">Controller configuration.</p>

    <FleetsSettings />
    <NotifierSettings />
    <EnrollmentSettings />
  </div>
</template>
```

- [ ] **Step 6: Build sanity + commit**

```bash
bun --cwd crates/isengard-plugins/dashboard/web run build

git add crates/isengard-plugins/dashboard/web/components/SettingsSection.vue \
        crates/isengard-plugins/dashboard/web/components/FleetsSettings.vue \
        crates/isengard-plugins/dashboard/web/components/NotifierSettings.vue \
        crates/isengard-plugins/dashboard/web/components/EnrollmentSettings.vue \
        crates/isengard-plugins/dashboard/web/pages/settings/index.vue
git commit -m "feat(dashboard-web): /settings page — fleets, notifiers, enrollment"
```

---

## Task 11: AddHostModal + wire AddHostButton

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/AddHostModal.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/AddHostButton.vue`
- Modify: `crates/isengard-plugins/dashboard/web/pages/hosts/index.vue` (open modal on click)

- [ ] **Step 1: Write AddHostModal.vue**

```vue
<script setup lang="ts">
import { useApi } from '~/composables/useApi'
import { useFleetsStore } from '~/stores/fleets'

defineEmits<{ close: [] }>()

const fleetsStore = useFleetsStore()
if (!fleetsStore.loaded) await fleetsStore.fetchAll()

const fleet = ref('default')
const hostname = ref('')
const installCommand = ref('')
const loading = ref(false)
const error = ref('')

async function generate() {
  loading.value = true
  error.value = ''
  try {
    const api = useApi()
    const dto = await api.post<{ install_command: string }>('/api/v1/hosts/enroll', {
      fleet: fleet.value,
      hostname: hostname.value || undefined,
    })
    installCommand.value = dto.install_command
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

async function copy() {
  await navigator.clipboard.writeText(installCommand.value)
}
</script>

<template>
  <div class="fixed inset-0 bg-black/60 flex items-center justify-center z-50" @click.self="$emit('close')">
    <div class="bg-iso-bg-base border border-iso-border rounded-lg w-[640px] max-w-full p-6 space-y-4">
      <header class="flex items-center justify-between">
        <h2 class="font-mono text-lg">Add host</h2>
        <button class="text-iso-text-muted hover:text-iso-text-base" @click="$emit('close')">
          <Icon name="lucide:x" :size="20" />
        </button>
      </header>

      <div v-if="!installCommand" class="space-y-3">
        <label class="block">
          <span class="text-xs uppercase tracking-wider text-iso-text-faint">Fleet</span>
          <select
            v-model="fleet"
            class="mt-1 w-full bg-iso-bg-elevated border border-iso-border rounded px-3 py-2 text-sm font-mono"
          >
            <option v-for="f in fleetsStore.items" :key="f.name" :value="f.name">{{ f.name }}</option>
          </select>
        </label>

        <label class="block">
          <span class="text-xs uppercase tracking-wider text-iso-text-faint">Hostname (optional)</span>
          <input
            v-model="hostname"
            type="text"
            placeholder="e.g. prod-04"
            class="mt-1 w-full bg-iso-bg-elevated border border-iso-border rounded px-3 py-2 text-sm font-mono"
          />
        </label>

        <button
          class="px-4 py-2 rounded border border-iso-border hover:border-iso-success hover:text-iso-success"
          :disabled="loading"
          @click="generate"
        >
          {{ loading ? 'Generating...' : 'Generate install command' }}
        </button>

        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </div>

      <div v-else class="space-y-3">
        <p class="text-sm text-iso-text-muted">
          Run this on the host you want to enroll. It will install the agent and contact this controller.
        </p>
        <pre class="text-xs font-mono bg-iso-bg-elevated rounded p-3 overflow-x-auto whitespace-pre-wrap">{{ installCommand }}</pre>
        <div class="flex items-center gap-2">
          <button class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-success" @click="copy">
            Copy
          </button>
          <button class="px-3 py-1.5 text-sm rounded border border-iso-border" @click="$emit('close')">
            Done
          </button>
        </div>
      </div>
    </div>
  </div>
</template>
```

- [ ] **Step 2: Wire AddHostButton to AddHostModal**

```vue
<!-- AddHostButton.vue -->
<script setup lang="ts">
const open = ref(false)
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-3 py-1.5 rounded border border-iso-border hover:border-iso-success hover:text-iso-success text-sm transition-colors"
    @click="open = true"
  >
    <Icon name="lucide:plus" :size="14" />
    Add host
  </button>
  <AddHostModal v-if="open" @close="open = false" />
</template>
```

(Drop the `@click` emit on the button since the modal is local. Update `pages/hosts/index.vue` to remove the `@click` handler that was pushing to `?add=1`.)

- [ ] **Step 3: Build + commit**

```bash
bun --cwd crates/isengard-plugins/dashboard/web run build

git add crates/isengard-plugins/dashboard/web/components/AddHostModal.vue \
        crates/isengard-plugins/dashboard/web/components/AddHostButton.vue \
        crates/isengard-plugins/dashboard/web/pages/hosts/index.vue
git commit -m "feat(dashboard-web): + AddHostModal with curl install command"
```

---

## Task 12: Cmd pane terminal mode — CmdTerminal + CmdBreadcrumb + xterm.js

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/CmdTerminal.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/CmdBreadcrumb.vue`
- Modify: `crates/isengard-plugins/dashboard/web/components/CmdPane.vue` (mount terminal when mode=terminal)
- Modify: `crates/isengard-plugins/dashboard/web/package.json` (+ xterm dep)

- [ ] **Step 1: Add xterm dependency**

```bash
bun --cwd crates/isengard-plugins/dashboard/web add xterm @xterm/addon-fit
```

This adds `xterm` and the fit addon to package.json + bun.lock. Commit those.

- [ ] **Step 2: Write CmdBreadcrumb.vue**

```vue
<script setup lang="ts">
interface Props {
  segments: string[]
  connected: boolean
}
defineProps<Props>()
defineEmits<{ 'toggle-position': []; close: [] }>()
</script>

<template>
  <header class="flex items-center justify-between px-3 py-2 border-b border-iso-border bg-iso-bg-elevated">
    <div class="flex items-center gap-2 text-xs font-mono">
      <Icon name="lucide:terminal" :size="14" class="text-iso-text-muted" />
      <span
        v-for="(seg, i) in segments"
        :key="i"
        class="flex items-center gap-1.5"
      >
        <span class="text-iso-text-muted">{{ seg }}</span>
        <span v-if="i < segments.length - 1" class="text-iso-text-faint">›</span>
      </span>
      <span class="ml-3 flex items-center gap-1.5">
        <span class="w-1.5 h-1.5 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-text-faint'" />
        <span class="text-iso-text-faint">{{ connected ? 'connected' : 'disconnected' }}</span>
      </span>
    </div>
    <div class="flex items-center gap-1">
      <button class="p-1 rounded hover:bg-iso-bg-base" :title="'Toggle position (⌘.)'" @click="$emit('toggle-position')">
        <Icon name="lucide:arrows-up-down" :size="14" />
      </button>
      <button class="p-1 rounded hover:bg-iso-bg-base" :title="'Close (⌘W)'" @click="$emit('close')">
        <Icon name="lucide:x" :size="14" />
      </button>
    </div>
  </header>
</template>
```

- [ ] **Step 3: Write CmdTerminal.vue**

```vue
<script setup lang="ts">
import { Terminal } from 'xterm'
import { FitAddon } from '@xterm/addon-fit'
import 'xterm/css/xterm.css'
import { useLogStream } from '~/composables/useLogStream'

interface Props {
  serviceId: string
  serviceName: string
  hostHostname: string
  fleet: string
  stackName?: string
}

const props = defineProps<Props>()
defineEmits<{ 'toggle-position': []; close: [] }>()

const containerEl = ref<HTMLElement | null>(null)
const term = new Terminal({
  fontFamily: 'JetBrains Mono, monospace',
  fontSize: 13,
  theme: {
    background: '#0a0e14',
    foreground: '#c5c8c6',
    cursor: '#7fdbca',
  },
  convertEol: true,
})
const fit = new FitAddon()
term.loadAddon(fit)

const { lines, connected, connect, disconnect } = useLogStream(props.serviceId)

onMounted(() => {
  if (containerEl.value) {
    term.open(containerEl.value)
    fit.fit()
    connect()
  }
})

onBeforeUnmount(() => {
  disconnect()
  term.dispose()
})

watch(lines, (newLines) => {
  // Append only the latest line (lines.value is push-only).
  const last = newLines[newLines.length - 1]
  if (last) {
    const color = last.stream === 'stderr' ? '\x1b[31m' : ''
    const reset = last.stream === 'stderr' ? '\x1b[0m' : ''
    term.writeln(`${color}${last.text}${reset}`)
  }
}, { deep: false })

const breadcrumb = computed(() => [
  'isengard',
  props.fleet,
  ...(props.stackName ? [props.stackName] : []),
  `${props.serviceName} @ ${props.hostHostname}`,
  'logs',
])
</script>

<template>
  <div class="flex flex-col h-full">
    <CmdBreadcrumb
      :segments="breadcrumb"
      :connected="connected"
      @toggle-position="$emit('toggle-position')"
      @close="$emit('close')"
    />
    <div ref="containerEl" class="flex-1 overflow-hidden" />
    <footer class="px-3 py-1.5 border-t border-iso-border bg-iso-bg-elevated text-[10px] text-iso-text-faint font-mono">
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘P</kbd> navigator ·
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘W</kbd> close ·
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘.</kbd> toggle position
    </footer>
  </div>
</template>
```

- [ ] **Step 4: Mount CmdTerminal inside CmdPane**

In `CmdPane.vue`, replace the placeholder terminal-mode block with:

```vue
<CmdTerminal
  v-if="ui.cmdPane.mode === 'terminal' && ui.cmdPane.terminal"
  :service-id="ui.cmdPane.terminal.serviceId"
  :service-name="ui.cmdPane.terminal.serviceName"
  :host-hostname="ui.cmdPane.terminal.hostHostname"
  :fleet="ui.cmdPane.terminal.fleet"
  :stack-name="ui.cmdPane.terminal.stackName"
  @toggle-position="ui.toggleCmdPosition()"
  @close="ui.closeCmdPane()"
/>
```

Extend the UI store from 5c to track terminal context:

```typescript
// stores/ui.ts (additions)
state: () => ({
  // ... existing 5c state ...
  cmdPane: {
    open: false,
    mode: 'navigator' as 'navigator' | 'terminal',
    position: 'center' as 'center' | 'docked',
    terminal: null as null | {
      serviceId: string
      serviceName: string
      hostHostname: string
      fleet: string
      stackName?: string
    },
  },
}),

actions: {
  openTerminalFor(svc: { id: string; name: string; hostHostname: string; fleet: string; stackName?: string }) {
    this.cmdPane.open = true
    this.cmdPane.mode = 'terminal'
    this.cmdPane.position = 'docked'
    this.cmdPane.terminal = {
      serviceId: svc.id,
      serviceName: svc.name,
      hostHostname: svc.hostHostname,
      fleet: svc.fleet,
      stackName: svc.stackName,
    }
  },
  toggleCmdPosition() {
    this.cmdPane.position = this.cmdPane.position === 'center' ? 'docked' : 'center'
  },
  closeCmdPane() {
    this.cmdPane.open = false
    this.cmdPane.terminal = null
    this.cmdPane.mode = 'navigator'
  },
},
```

- [ ] **Step 5: Wire navigator → terminal selection**

In the cmd pane navigator (extended in 5d Task 13), when a result of type `service` is selected, dispatch `ui.openTerminalFor({...})` instead of `router.push(...)`. Add an explicit "Tail logs on X" action in the ACTIONS section that always opens terminal mode.

- [ ] **Step 6: Build + commit**

```bash
bun --cwd crates/isengard-plugins/dashboard/web run build

git add crates/isengard-plugins/dashboard/web/components/CmdTerminal.vue \
        crates/isengard-plugins/dashboard/web/components/CmdBreadcrumb.vue \
        crates/isengard-plugins/dashboard/web/components/CmdPane.vue \
        crates/isengard-plugins/dashboard/web/stores/ui.ts \
        crates/isengard-plugins/dashboard/web/package.json \
        crates/isengard-plugins/dashboard/web/bun.lock
git commit -m "feat(dashboard-web): + CmdTerminal w/ xterm.js + CmdBreadcrumb; navigator opens terminal mode"
```

---

## Task 13: `/install.sh` endpoint — serve agent install script bound to enrollment token

**Files:**
- Create: `crates/isengard-plugins/dashboard/templates/install.sh.tmpl`
- Modify: `crates/isengard-plugins/dashboard/src/api.rs` (add handler)
- Modify: `crates/isengard-plugins/dashboard/src/lib.rs` (route registration)

The Add Host modal generates `curl -fsSL <controller>/install.sh | sh -s -- --token <token>`. This task creates the actual `/install.sh` endpoint that responds to that curl. The server reads the token query (or first script arg), looks it up in the `settings` table (`enrollment.token.<token>`), and renders a bash script that downloads the agent binary and registers with the controller.

- [ ] **Step 1: Write the failing test**

Add to `api.rs` test module:

```rust
#[tokio::test]
async fn install_sh_with_valid_token_returns_bash_script() {
    let inv = Inventory::open_in_memory().await.unwrap();
    inv.set_setting(
        "enrollment.token.testtoken",
        &serde_json::json!({
            "agent_id": "01HX0000000000000000000001",
            "fleet": "staging",
            "hostname": null,
        }),
    ).await.unwrap();

    let app = build_router(test_handles(inv));
    let resp = app.oneshot(
        Request::builder()
            .uri("/install.sh?token=testtoken")
            .body(Body::empty()).unwrap()
    ).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/x-shellscript; charset=utf-8");
    let body = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.starts_with("#!/usr/bin/env bash"));
    assert!(body_str.contains("ISENGARD_TOKEN=testtoken"));
    assert!(body_str.contains("ISENGARD_FLEET=staging"));
}

#[tokio::test]
async fn install_sh_with_missing_token_returns_400() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let app = build_router(test_handles(inv));
    let resp = app.oneshot(
        Request::builder()
            .uri("/install.sh")
            .body(Body::empty()).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn install_sh_with_unknown_token_returns_403() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let app = build_router(test_handles(inv));
    let resp = app.oneshot(
        Request::builder()
            .uri("/install.sh?token=nope")
            .body(Body::empty()).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), 403);
}
```

- [ ] **Step 2: Run failing tests**

Run: `cargo test -p isengard-dashboard install_sh --no-run`
Expected: FAIL — handler doesn't exist.

- [ ] **Step 3: Create the install script template**

Create `crates/isengard-plugins/dashboard/templates/install.sh.tmpl`:

```bash
#!/usr/bin/env bash
# Isengard agent installer — generated by controller at {{controller_url}}
# Token: {{token}} (single-use, expires in 30 min)
set -euo pipefail

ISENGARD_CONTROLLER="{{controller_url}}"
ISENGARD_TOKEN="{{token}}"
ISENGARD_FLEET="{{fleet}}"
ISENGARD_HOSTNAME="${ISENGARD_HOSTNAME:-{{hostname_or_default}}}"

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)  ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
  linux) ;;
  *) echo "unsupported os: $OS" >&2; exit 1 ;;
esac

INSTALL_DIR="/usr/local/bin"
TMP="$(mktemp -d)"
trap "rm -rf $TMP" EXIT

echo "Downloading isengard-agent for $OS-$ARCH..."
curl -fsSL "$ISENGARD_CONTROLLER/dist/isengard-agent-$OS-$ARCH" -o "$TMP/isengard-agent"
chmod +x "$TMP/isengard-agent"

if [ "$(id -u)" -eq 0 ]; then
  mv "$TMP/isengard-agent" "$INSTALL_DIR/isengard-agent"
else
  sudo mv "$TMP/isengard-agent" "$INSTALL_DIR/isengard-agent"
fi

echo "Enrolling with controller..."
"$INSTALL_DIR/isengard-agent" enroll \
  --controller "$ISENGARD_CONTROLLER" \
  --token "$ISENGARD_TOKEN" \
  --fleet "$ISENGARD_FLEET" \
  ${ISENGARD_HOSTNAME:+--hostname "$ISENGARD_HOSTNAME"}

echo "Installing systemd unit..."
"$INSTALL_DIR/isengard-agent" install-service

echo "Done. Agent running. View it at $ISENGARD_CONTROLLER"
```

(Embed via `include_str!` in the handler.)

- [ ] **Step 4: Implement the handler**

```rust
#[derive(Debug, Deserialize)]
pub struct InstallShQuery {
    pub token: Option<String>,
}

const INSTALL_SH_TEMPLATE: &str = include_str!("../templates/install.sh.tmpl");

pub async fn install_sh(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<InstallShQuery>,
) -> Result<Response, ApiError> {
    let token = q.token.ok_or(ApiError::BadRequest("missing token query".into()))?;

    // Token must basic-validate before we hit the DB (avoid SQL on garbage).
    if token.len() != 26 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::Forbidden("invalid token format".into()));
    }

    let key = format!("enrollment.token.{token}");
    let entry = handles.inventory.get_setting(&key).await?
        .ok_or(ApiError::Forbidden("token not found or already used".into()))?;

    let fleet = entry.get("fleet").and_then(|v| v.as_str()).unwrap_or("default");
    let hostname = entry.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
    let controller_url = handles.config.public_url.clone();

    let body = INSTALL_SH_TEMPLATE
        .replace("{{controller_url}}", &controller_url)
        .replace("{{token}}", &token)
        .replace("{{fleet}}", fleet)
        .replace("{{hostname_or_default}}", hostname);

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        body,
    ).into_response())
}
```

Add `Forbidden(String)` variant to `ApiError` if missing.

- [ ] **Step 5: Register the route**

In `lib.rs`, add to the router:

```rust
.route("/install.sh", get(crate::api::install_sh))
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p isengard-dashboard install_sh`
Expected: PASS — all three tests green.

- [ ] **Step 7: Commit**

```bash
git add crates/isengard-plugins/dashboard/templates/install.sh.tmpl \
        crates/isengard-plugins/dashboard/src/api.rs \
        crates/isengard-plugins/dashboard/src/lib.rs
git commit -m "feat(dashboard): /install.sh endpoint — serves agent installer bound to enrollment token"
```

---

## Task 14: Toast notifications system

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/Toast.vue`
- Create: `crates/isengard-plugins/dashboard/web/components/ToastContainer.vue`
- Create: `crates/isengard-plugins/dashboard/web/composables/useToast.ts`
- Create: `crates/isengard-plugins/dashboard/web/stores/toasts.ts`
- Modify: `crates/isengard-plugins/dashboard/web/layouts/default.vue` (mount ToastContainer)
- Modify: pages that perform mutations (Settings, Hosts, Stack detail, Add host modal) — call `toast.success/error/info`

Mutations need feedback. Toast is a small bottom-right panel that fades in for ~4s. Three kinds: success (green), error (red), info (neutral). A queue of up to 3 visible at once; new ones push old ones up; dismissable via X.

- [ ] **Step 1: Write the toasts store**

```typescript
// crates/isengard-plugins/dashboard/web/stores/toasts.ts
import { defineStore } from 'pinia'

export type ToastKind = 'success' | 'error' | 'info'

export interface Toast {
  id: number
  kind: ToastKind
  text: string
  expiresAt: number
}

let nextId = 1

export const useToastsStore = defineStore('toasts', {
  state: () => ({
    items: [] as Toast[],
  }),

  actions: {
    push(kind: ToastKind, text: string, durationMs = 4000) {
      const t: Toast = {
        id: nextId++,
        kind,
        text,
        expiresAt: Date.now() + durationMs,
      }
      this.items.push(t)
      // Cap visible to 3
      if (this.items.length > 3) this.items.splice(0, this.items.length - 3)
      // Auto-dismiss
      setTimeout(() => this.dismiss(t.id), durationMs)
    },

    dismiss(id: number) {
      this.items = this.items.filter((t) => t.id !== id)
    },
  },
})
```

- [ ] **Step 2: Write the useToast composable**

```typescript
// crates/isengard-plugins/dashboard/web/composables/useToast.ts
import { useToastsStore } from '~/stores/toasts'

export function useToast() {
  const store = useToastsStore()
  return {
    success: (text: string) => store.push('success', text),
    error:   (text: string) => store.push('error', text),
    info:    (text: string) => store.push('info', text),
  }
}
```

- [ ] **Step 3: Write Toast.vue**

```vue
<script setup lang="ts">
import type { Toast } from '~/stores/toasts'
import { useToastsStore } from '~/stores/toasts'

defineProps<{ toast: Toast }>()
const store = useToastsStore()

const kindClasses = {
  success: 'border-iso-success bg-iso-success/10 text-iso-success',
  error:   'border-iso-error   bg-iso-error/10   text-iso-error',
  info:    'border-iso-border  bg-iso-bg-overlay text-iso-text-secondary',
}

const kindIcons = {
  success: 'lucide:check-circle',
  error:   'lucide:x-circle',
  info:    'lucide:info',
}
</script>

<template>
  <div
    class="flex items-center gap-3 px-4 py-3 rounded-lg border min-w-[280px] max-w-[420px] shadow-lg"
    :class="kindClasses[toast.kind]"
  >
    <Icon :name="kindIcons[toast.kind]" :size="16" class="shrink-0" />
    <span class="text-sm flex-1">{{ toast.text }}</span>
    <button class="opacity-60 hover:opacity-100" @click="store.dismiss(toast.id)">
      <Icon name="lucide:x" :size="14" />
    </button>
  </div>
</template>
```

- [ ] **Step 4: Write ToastContainer.vue**

```vue
<script setup lang="ts">
import { useToastsStore } from '~/stores/toasts'

const store = useToastsStore()
</script>

<template>
  <div class="fixed bottom-12 right-4 z-50 flex flex-col gap-2 pointer-events-auto">
    <Toast
      v-for="t in store.items"
      :key="t.id"
      :toast="t"
    />
  </div>
</template>
```

- [ ] **Step 5: Mount in default layout**

Edit `layouts/default.vue` (create if missing):

```vue
<template>
  <div class="h-screen flex flex-col">
    <slot />
    <ToastContainer />
  </div>
</template>
```

- [ ] **Step 6: Wire mutations to toast**

In every mutation site:

```typescript
// hosts/index.vue handleAction force-update branch
const toast = useToast()
try {
  await actions.forceUpdate(host.id)
  toast.success(`Force update queued for ${host.hostname}`)
} catch (e) {
  toast.error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
}
```

Apply the same pattern in:
- `HostInspector.vue` → setFleet, forceUpdate, decommission
- `FleetsSettings.vue` → create, remove
- `NotifierSettings.vue` → patch (settings save)
- `AddHostModal.vue` → generate (enrollment created)
- Stack detail → forceUpdate

- [ ] **Step 7: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/Toast.vue \
        crates/isengard-plugins/dashboard/web/components/ToastContainer.vue \
        crates/isengard-plugins/dashboard/web/composables/useToast.ts \
        crates/isengard-plugins/dashboard/web/stores/toasts.ts \
        crates/isengard-plugins/dashboard/web/layouts/default.vue \
        crates/isengard-plugins/dashboard/web/components/HostInspector.vue \
        crates/isengard-plugins/dashboard/web/components/FleetsSettings.vue \
        crates/isengard-plugins/dashboard/web/components/NotifierSettings.vue \
        crates/isengard-plugins/dashboard/web/components/AddHostModal.vue \
        crates/isengard-plugins/dashboard/web/pages/hosts/index.vue \
        crates/isengard-plugins/dashboard/web/pages/stacks/[id].vue
git commit -m "feat(dashboard-web): toast notifications + wire into all mutations"
```

---

## Task 15: ConfirmDialog for destructive actions

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/components/ConfirmDialog.vue`
- Create: `crates/isengard-plugins/dashboard/web/composables/useConfirm.ts`
- Modify: `HostInspector.vue` (decommission), `FleetsSettings.vue` (delete fleet)

Replace the native `confirm()` calls with a styled in-app modal that matches the AddHostModal pattern. Body: title + description + danger callout + Cancel/Confirm. Confirm button is red-bordered. Returns a promise that resolves true/false.

- [ ] **Step 1: Write ConfirmDialog.vue**

```vue
<script setup lang="ts">
interface Props {
  open: boolean
  title: string
  description: string
  confirmText?: string
  danger?: boolean
}

withDefaults(defineProps<Props>(), {
  confirmText: 'Confirm',
  danger: false,
})

defineEmits<{ resolve: [confirmed: boolean] }>()
</script>

<template>
  <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center bg-black/60" @click.self="$emit('resolve', false)">
    <div class="bg-iso-bg-base border border-iso-border rounded-lg w-[460px] max-w-full p-6 space-y-4">
      <h2 class="font-mono text-base">{{ title }}</h2>
      <p class="text-sm text-iso-text-muted">{{ description }}</p>
      <div class="flex justify-end gap-2 pt-2">
        <button
          class="px-3 py-1.5 text-sm rounded border border-iso-border hover:border-iso-text-muted"
          @click="$emit('resolve', false)"
        >
          Cancel
        </button>
        <button
          class="px-3 py-1.5 text-sm rounded border"
          :class="danger
            ? 'border-iso-error text-iso-error hover:bg-iso-error/10'
            : 'border-iso-success text-iso-success hover:bg-iso-success/10'"
          @click="$emit('resolve', true)"
        >
          {{ confirmText }}
        </button>
      </div>
    </div>
  </div>
</template>
```

- [ ] **Step 2: Write useConfirm composable**

```typescript
// crates/isengard-plugins/dashboard/web/composables/useConfirm.ts
import { ref } from 'vue'

interface ConfirmOpts {
  title: string
  description: string
  confirmText?: string
  danger?: boolean
}

const open = ref(false)
const opts = ref<ConfirmOpts>({ title: '', description: '' })
let resolver: ((confirmed: boolean) => void) | null = null

export function useConfirm() {
  function confirm(o: ConfirmOpts): Promise<boolean> {
    opts.value = o
    open.value = true
    return new Promise((resolve) => { resolver = resolve })
  }

  function resolve(confirmed: boolean) {
    open.value = false
    resolver?.(confirmed)
    resolver = null
  }

  return { open, opts, confirm, resolve }
}
```

- [ ] **Step 3: Mount globally in default layout**

Edit `layouts/default.vue`:

```vue
<script setup lang="ts">
const { open, opts, resolve } = useConfirm()
</script>

<template>
  <div class="h-screen flex flex-col">
    <slot />
    <ToastContainer />
    <ConfirmDialog
      :open="open"
      :title="opts.title"
      :description="opts.description"
      :confirm-text="opts.confirmText"
      :danger="opts.danger"
      @resolve="resolve"
    />
  </div>
</template>
```

- [ ] **Step 4: Replace native `confirm()` calls**

In `HostInspector.vue`:

```typescript
async function decommission() {
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: `Decommission ${props.host.hostname}?`,
    description: 'This revokes its enrollment token and removes it from inventory. The agent on the host will no longer report. This cannot be undone.',
    confirmText: 'Decommission',
    danger: true,
  })
  if (!ok) return

  const toast = useToast()
  try {
    await actions.decommission(props.host.id)
    toast.success(`${props.host.hostname} decommissioned`)
    emit('close')
    emit('changed')
  } catch (e) {
    toast.error(`Decommission failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}
```

In `FleetsSettings.vue`:

```typescript
async function remove(name: string) {
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: `Delete fleet "${name}"?`,
    description: 'The fleet tag is removed. Hosts assigned to this fleet must be reassigned first.',
    confirmText: 'Delete',
    danger: true,
  })
  if (!ok) return

  const toast = useToast()
  try {
    await fleetsStore.remove(name)
    toast.success(`Fleet "${name}" deleted`)
  } catch (e) {
    toast.error(e instanceof Error ? e.message : String(e))
  }
}
```

- [ ] **Step 5: Build sanity**

Run: `bun --cwd crates/isengard-plugins/dashboard/web run build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isengard-plugins/dashboard/web/components/ConfirmDialog.vue \
        crates/isengard-plugins/dashboard/web/composables/useConfirm.ts \
        crates/isengard-plugins/dashboard/web/layouts/default.vue \
        crates/isengard-plugins/dashboard/web/components/HostInspector.vue \
        crates/isengard-plugins/dashboard/web/components/FleetsSettings.vue
git commit -m "feat(dashboard-web): ConfirmDialog for decommission + delete-fleet (replaces native confirm)"
```

---

## Task 16: Final CI gate, end-to-end smoke, both tags

**Files:**
- (none — verification + tagging)

- [ ] **Step 1: Run full local CI**

```bash
just ci-local
```

Expected: PASS — entire workspace builds, all tests pass, no lints, deny clean, web bundle builds.

- [ ] **Step 2: End-to-end smoke**

Run controller + agent + a labelled compose stack as in 5d. Then:

- Browser: `/events` → toggle FAILED chip → only failed events visible
- Browser: `/settings` → click "+ New fleet" → enter `prod` → it appears in the list
- Browser: `/settings` → toggle Telegram → settings round-trips
- Browser: `/hosts` → click "+ Add host" → modal opens → click "Generate" → curl command appears with a token; copy it
- Browser: `⌘K` → type `web` → see the service in results → press Enter → terminal mode opens docked, logs stream live
- Browser: `⌘.` → terminal pane recenters
- Browser: click "Force update" on host → action enqueued; observe agent picks it up next heartbeat (agent stdout shows `force_update dispatched`); within ~10s the timeline shows `dashboard.action_dispatched` followed by `update.checked`/`update.success`/`update.failed` events

- [ ] **Step 3: Tag 5e + Phase 5 complete**

```bash
git tag -a v0.1.0-alpha.phase5e          -m "Phase 5e: events + settings + add-host + cmd pane terminal"
git tag -a v0.1.0-alpha.phase5-complete  -m "Phase 5 complete: dashboard end-to-end"
```

- [ ] **Step 4: Verify no push happened**

```bash
git status -sb
git tag -l | tail -5
```

Expected: branch ahead of `origin/next`, both tags present locally.

DO NOT `git push`. The user explicitly maintains the no-push rule.

---

## Self-review

1. **Spec coverage:**
   - Services persistence: ✅ Tasks 1-2
   - host_actions queue + RPC pipe: ✅ Tasks 1-2-3-4-5
   - Real enroll: ✅ Task 6
   - Real fleet CRUD: ✅ Task 6
   - Settings CRUD: ✅ Task 6
   - WS log streaming: ✅ Task 7
   - useLogStream + settings/fleets stores: ✅ Task 8
   - Events page: ✅ Task 9
   - Settings page: ✅ Task 10
   - AddHostModal: ✅ Task 11
   - CmdTerminal + xterm.js: ✅ Task 12
   - Final smoke + Phase 5 tag: ✅ Task 13

2. **Placeholder scan:** No `TBD`. Two scoped-down items are flagged explicitly:
   - Bidirectional shell (`docker exec -i`) is intentionally read-only logs in v1; spec out-of-scope.
   - Enrollment tokens stored in `settings` table (single row per token); v1.x adds a dedicated `enrollment_tokens` table with TTL.

3. **Type consistency:**
   - `ServiceId(i64)` consistent across storage + DTOs (DTO serializes as decimal `String`)
   - `HostActionKind` (snake_case `kind` tag) consistent across storage serialize, proto wire (`payload_json`), and agent deserialize
   - `Fleet.name` is `String` everywhere; FleetDto adds `host_count: u32` derived in handler
   - `Service.stack_id: Option<StackId>` matches both heartbeat (`ServiceInfo.stack` resolved server-side) and frontend (`stack_id: string | null`)

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-01-phase-5e-events-settings-shell.md`.

**1. Subagent-Driven (recommended)** — fresh subagent per task, two-stage review, fast iteration.

**2. Inline Execution** — execute tasks in this session via executing-plans, batch with checkpoints.

Which approach?
