# Phase 8 Plan C: Settings UI Networking Tab + Atomic Upstream Swap Design Spec

**Status:** Final, 2026-05-04
**Parent spec:** `2026-05-03-phase-8-networking-and-proxy-design.md` — Plan C picks up at §8 sub-phases 8h, 8i
**Predecessors:** Plan A (`feat/networking-proxy-core`, PR #18), Plan B (`feat/networking-tls-adapters`, PR #19)
**Pencil source:** `design/concepts/settings-networking/{v1,adapter-cf-tunnel-v1,adapter-tailscale-v1,routing-rule-edit-v1,service-expose-v1}.html`
**Author:** AI partner (Opus 4.7), in dialogue with engineer

---

## 1. Why this exists

Plan A made Pingora route. Plan B added TLS + the two real adapters. Both work end-to-end at the protocol level — but configuring routing rules and adapters today still requires writing SQL into the controller's database. That's not the product.

Plan C closes that gap on two fronts:

- **8h**: a Networking tab in the dashboard that exposes the `routing_rules` + `adapter_config` storage as a CRUD UI. The 5 design concepts in `design/concepts/settings-networking/` are the visual contract; this spec ports them to Vue components and adds the backing HTTP API in the dashboard plugin.
- **8i**: an atomic upstream swap API on the agent's `ProxyState`. Internal Rust only — no UI, no HTTP. Phase 10 (Blue-Green Deployment) is the consumer; Plan C ships the primitive so 10's orchestration has something to call.

After Plan C, an operator can manage the proxy entirely from the dashboard, and the proxy itself supports the drain-and-swap semantics blue-green needs.

---

## 2. Architecture

### 8h Settings UI

The current `/settings` route renders a single scrolling page with three sibling sections (`FleetsSettings`, `NotifierSettings`, `EnrollmentSettings`). Plan C **converts this to tabs.** The decision was deliberate during brainstorming — sections will keep accumulating (Networking now, Policies + Webhooks + Backup + Notifier later) and the scrolling-page layout doesn't scale. Tabs sort early.

```
/settings                            (URL stays the same; tab is hash-driven OR query-param)
├── #general            (was FleetsSettings + EnrollmentSettings, grouped)
├── #networking         (NEW — adapter cards + routing rules table)
├── #notifier           (was NotifierSettings)
└── (future tabs slot in here)
```

URL strategy: tab state lives in `?tab=networking` query param (NOT a route segment), so deep links work and refresh keeps the active tab. Default tab when no param: `general`.

### Tab container

`SettingsTabs.vue` is a thin component:
- Reads `route.query.tab` to determine the active tab
- Updates `?tab=` on tab clicks via `router.push({ query: { tab } })`
- Provides scoped slot per tab so each tab's body is a sibling component

The existing three sections are wrapped:
- `general` tab: contains `FleetsSettings` + `EnrollmentSettings` (these conceptually overlap)
- `notifier` tab: contains `NotifierSettings`
- `networking` tab: NEW — contains the new components

### Networking tab body

Single Vue component `NetworkingSettings.vue` that hosts:

1. **Section: Adapters** — cards for the three installed adapters (cf-tunnel, tailscale, none). Each card shows adapter status + configuration form. Cards are independent components (`AdapterCardCfTunnel.vue`, `AdapterCardTailscale.vue`, `AdapterCardNone.vue`) so adding a new adapter type later is a 1-file change.
2. **Section: Routing rules** — table with one row per rule. Source column shows where the rule came from (UI / Label / Imported). Inline edit via `RoutingRuleEditModal.vue`. "+ Add rule" button opens the same modal in create mode.

### 8i Atomic upstream swap

A new `proxy::swap` module on the agent. Adds:

- `Upstream::state: UpstreamState` enum — `Active`, `Draining`, `Removed`
- `UpstreamRegistry::set_state(hostname, state)` — mutate state without touching addr/health
- `swap_upstream(state, hostname, new_upstream, grace_period)` — public function on `proxy` module:
  1. Mark the existing entry for `hostname` as `Draining`
  2. Insert `new_upstream` with `state: Active` (replaces the entry — registry is hostname-keyed)
  3. Wait `grace_period` (caller's tokio task)
  4. Remove the draining entry from the registry

Pingora's `IsengardProxy::upstream_peer` already short-circuits unhealthy upstreams; we add a similar guard to skip `Draining` upstreams (no new connections sent to them). Existing in-flight connections continue against the upstream they were already routed to — Pingora's per-connection state holds the upstream pointer, not a per-request lookup.

This delivers what the spec §4 calls out: *"Atomic upstream swap (blue-green hook). Pingora exposes an internal `swap_upstream(rule_id, new_upstream)` API callable by the agent."* — the only adjustment is the function takes `hostname` (the registry key), not `rule_id` (the swap is local to the agent's view; rule_id round-trips would require the controller in the loop unnecessarily for Phase 10's per-host orchestration).

---

## 3. HTTP API endpoints

All new endpoints live in `crates/isengard-plugins/dashboard/src/api/routing.rs` (new module under the existing `api/` directory pattern). Wired into the existing axum router via `crates/isengard-plugins/dashboard/src/api/mod.rs`. Auth uses the existing dashboard token middleware (already protects other endpoints).

```
GET    /api/routing/rules
       → 200 [RoutingRule, ...]   (returns rules for the active fleet, ordered by id ASC)

POST   /api/routing/rules
       Body: InsertRoutingRule (JSON, source defaults to "ui" if absent)
       → 201 RoutingRule          (newly inserted)
       → 409 if (public_hostname, host_id) UNIQUE conflict

PATCH  /api/routing/rules/:id
       Body: partial update (any subset of: container_port, healthcheck_path, healthcheck_interval_secs, adapter, tls_mode, auth, state, healthcheck-related)
       → 200 RoutingRule          (post-update)
       → 404 if id not found

DELETE /api/routing/rules/:id
       → 204
       → 404 if id not found

GET    /api/routing/rules/:id/overrides
       → 200 [RoutingRuleOverride, ...]

PUT    /api/routing/rules/:id/overrides/:field
       Body: { value_json: any }
       → 200 RoutingRuleOverride  (upsert)

GET    /api/networking/adapter-config/:host_id/:adapter
       → 200 AdapterConfig
       → 404 if host_id+adapter pair not found

PUT    /api/networking/adapter-config/:host_id/:adapter
       Body: UpsertAdapterConfig (config_json + enabled)
       → 200 AdapterConfig         (upsert)
```

Error shape: existing dashboard convention (`{ "error": "string", "code": "snake_case" }` + appropriate HTTP status).

The existing `crates/isengard-plugins/dashboard/src/api.rs` module is already large; this is a good moment to split it into `api/mod.rs` + per-resource files (`hosts.rs`, `events.rs`, etc.) if it's grown unwieldy. Plan C stays scoped: only `routing.rs` is added new; the existing big `api.rs` is left alone unless touched by routing-rule lookups (which would need to read `routing_rules` from the same Inventory the existing endpoints already use).

### Notably absent from this round

- No bulk import endpoint (NPM/Traefik) — separate plan
- No per-rule history / audit log endpoint — defer until needed
- No streaming (SSE/WebSocket) for routing rule changes — clients poll on focus / explicit reload, matching the existing convention

---

## 4. Adapter card UX details

Each adapter card lives in its own component. Common pattern:

```
┌─────────────────────────────────────┐
│ <icon> cf-tunnel       <toggle>     │
│ <status pill: connected | error>    │
├─────────────────────────────────────┤
│ <config form>                       │
│  - api_token (password-masked)      │
│  - account_id                       │
│  - zone_id                          │
│  - tunnel_name                      │
│  - tunnel_id (read-only after join) │
├─────────────────────────────────────┤
│ <actions: Save | Test connection>   │
└─────────────────────────────────────┘
```

### Password masking

- API token field is `<input type="password">` by default
- Eye-icon toggle reveals it (sets `type="text"` for the focused field)
- On save, the value is sent in the PUT body as plain text over HTTPS (the controller is HTTPS-protected; we're not building TLS-terminated-in-app secret flow)
- Storage: written into `adapter_config.config_json` as plain text JSON. SQLite at rest.

This is "password manager" tier security, not "secrets vault" tier. The decision in brainstorming was explicit: keychain integration is too platform-specific for v1, env-var-only is bad UX, plain text + masked input is the pragmatic middle. Document this clearly in the adapter card UI: a small note like "Tokens are stored on the controller; protect controller access accordingly."

### "Test connection" button

For cf-tunnel: makes a `GET /accounts/<account_id>/cfd_tunnel?per_page=1` API call to validate the token works (counts against the user's CF rate limit but it's 1200/5min so fine).

For tailscale: runs `tailscale status --json` via the agent and returns the parsed status to the UI. (This requires a new endpoint to invoke an adapter test — keep it minimal: `POST /api/networking/adapter-config/:host_id/:adapter/test` returns `{ ok: bool, error: Option<String>, detail: serde_json::Value }`.)

For none: no-op, always returns `ok: true`.

### Status pills (top of each card)

Color-coded based on the most recent test result + recent activity from the agent's heartbeat:
- Green "connected" — recent success
- Yellow "warning" — last test was >24h ago OR partial config
- Red "error" — last test failed
- Gray "not configured" — no adapter_config row yet for this host

---

## 5. Routing rules table UX

Single table component `RoutingRulesTable.vue` lives in the Networking tab. Pencil mock is `design/concepts/settings-networking/v1.html` — the table with SOURCE column.

Columns (left → right):
- Hostname (sortable; primary key visually)
- Target (`<service>:<port>`)
- Adapter (cf-tunnel / tailscale / none)
- TLS (edge / acme / manual — color-coded badge)
- Health (✓ healthy / ⚠ degraded / ✗ failed — derived from `routing.upstream.health_changed` events + the rule's `state` field)
- Source (🏷 label · `<container_name>` / 🎨 ui / 📥 imported)
- Actions (... menu: Edit / Delete / Convert to UI rule [if source=label])

Behaviors:
- Click on a label-source row → modal explains "this rule comes from container `<X>`'s `isengard.expose` label. Remove the label to stop using it, or click 'Convert to UI rule' to take ownership."
- Click on a UI-source row → opens RoutingRuleEditModal pre-populated.
- "+ Add rule" button → opens the modal in create mode (defaults: source=ui, tls_mode=acme, adapter=settings default for current host).
- Health column auto-updates from the existing event stream (the dashboard already subscribes to events via `composables/useEvents.ts`).

### RoutingRuleEditModal

Port of `design/concepts/settings-networking/routing-rule-edit-v1.html`. Fields:
- Hostname (validated: non-empty, looks like a domain)
- Target service + container port (dropdown of services + numeric input)
- Adapter (radio: none / tailscale / cf-tunnel)
- TLS mode (radio: edge / acme / manual)
- Healthcheck path (optional text; empty = TCP-only)
- Healthcheck interval (numeric, default 10s)
- Auth (parsed but UI-noted as "v1.x enforcement" — don't even surface `cf-access` option until enforcement lands)

Cancel / Save buttons. On save, `POST /api/routing/rules` (create) or `PATCH /api/routing/rules/:id` (edit).

### ServiceExposeModal

Port of `design/concepts/settings-networking/service-expose-v1.html`. Opens from Stack detail's "Expose" button. Pre-populated with the selected service + first exposed port. Same form as RoutingRuleEditModal but with the service/port locked. Source defaults to `ui`.

This modal belongs in `crates/isengard-plugins/dashboard/web/components/ServiceExposeModal.vue` and is invoked from `pages/stacks/[id].vue` (existing).

---

## 6. 8i Atomic upstream swap design

### State machine

```
   ┌─────────────┐    swap_upstream()    ┌──────────────┐
   │   Active    │ ────────────────────▶ │   Draining   │
   └─────────────┘                       └──────────────┘
                                                │
                                                │ grace_period elapses
                                                ▼
                                         ┌──────────────┐
                                         │   Removed    │
                                         │ (drop entry) │
                                         └──────────────┘
```

### Files

- `crates/isengard-agent/src/proxy/upstreams.rs` — extend `Upstream` with `state: UpstreamState`. Add `UpstreamRegistry::set_state(hostname, state)` and `UpstreamRegistry::remove_if_draining(hostname)`.
- `crates/isengard-agent/src/proxy/swap.rs` (new) — `pub async fn swap_upstream(state: &ProxyState, hostname: &str, new_upstream: Upstream, grace_period: Duration)`.
- `crates/isengard-agent/src/proxy/router.rs` — `IsengardProxy::upstream_peer` adds a guard: skip upstreams where `state == Draining`.
- `crates/isengard-agent/src/proxy/mod.rs` — re-export `swap::swap_upstream`.

### swap_upstream semantics

```rust
pub async fn swap_upstream(
    state: &ProxyState,
    hostname: &str,
    new_upstream: Upstream,
    grace_period: Duration,
) -> Result<()> {
    // 1. Mark current as draining (no new connections will be routed to it).
    {
        let mut w = state.upstreams.write().await;
        if let Some(up) = w.get_mut(hostname) {
            up.state = UpstreamState::Draining;
        }
    }

    // 2. Spawn the drain timer. After grace_period:
    //    - If the entry's state is still Draining, remove it.
    //    - If the new_upstream got installed in the meantime (step 3 below
    //      happens synchronously after this swap call), the entry was
    //      replaced and there's nothing draining to clean up.
    let st = state.clone();
    let host = hostname.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(grace_period).await;
        let mut w = st.upstreams.write().await;
        if let Some(up) = w.get(&host) {
            if up.state == UpstreamState::Draining {
                w.remove(&host);
            }
        }
    });

    // 3. Install the new upstream. UpstreamRegistry::set replaces the entry.
    let mut new_active = new_upstream;
    new_active.state = UpstreamState::Active;
    state.upstreams.write().await.set(hostname.to_string(), new_active);

    Ok(())
}
```

### Why no rule_id

The spec §4 originally suggested `swap_upstream(rule_id, new_upstream)` taking a rule ID. Plan C uses `hostname` instead because:
- The agent's `UpstreamRegistry` is keyed by hostname, not rule ID
- Rule ID would require the agent to map rule_id → hostname via storage lookup, adding latency and a failure mode for no real benefit
- Phase 10's blue-green orchestration calls this from the agent process where it already has the hostname

If a future caller has only a rule_id, they look up the rule's hostname and call `swap_upstream(hostname, ...)`.

### Tests

- `crates/isengard-agent/tests/proxy_swap_unit.rs`:
  - `swap_marks_old_as_draining_then_installs_new`
  - `draining_upstream_removed_after_grace_period`
  - `router_skips_draining_upstreams_for_new_connections` (uses Pingora end-to-end, similar shape to `proxy_basic_routing.rs`)

---

## 7. Out of scope (later)

- **Phase 9 Update Policies** (Approval flow, layered policy model) — separate spec round
- **Phase 10 Blue-Green Deployment** — uses the swap_upstream primitive; deserves its own spec
- DNS-01 ACME (still v1.x — HTTP-01 covers v1.x cleanly)
- Bulk import (NPM JSON, Traefik dynamic.yml) — separate spec round
- mTLS to upstream containers, HTTP/3 — v1.x
- Per-rule auth enforcement (`cf-access`, basic auth) — v1.x
- Tailscale custom-domain (CNAME-to-tailnet) support — v1.x
- Headscale, raw-wireguard, custom adapters — separate plugin specs

---

## 8. Phasing

| Sub-phase | Scope | Why this order |
|---|---|---|
| **8h-1** | New REST endpoints in `dashboard/src/api/routing.rs` (rules CRUD + adapter config + test endpoint) | API surface ready before any UI consumes it |
| **8h-2** | `SettingsTabs.vue` container + refactor existing settings sections (general, notifier) into tabs | Restructure first so Networking lands as a new tab cleanly |
| **8h-3** | `NetworkingSettings.vue` shell + `RoutingRulesTable.vue` + `useRoutingRules.ts` composable | First user-facing surface: see + manage existing rules |
| **8h-4** | `RoutingRuleEditModal.vue` (create + edit) + `ServiceExposeModal.vue` (from Stack detail) | Full rule lifecycle UX |
| **8h-5** | Three adapter cards (`AdapterCardCfTunnel`, `AdapterCardTailscale`, `AdapterCardNone`) + `useAdapterConfig.ts` composable + test-connection endpoint | Adapter management UX |
| **8i-1** | `Upstream.state` field + registry `set_state`/`remove_if_draining` methods | Foundation for swap |
| **8i-2** | `proxy/swap.rs` + `swap_upstream` function + router skip-draining guard + 3 unit tests | The swap primitive |

Each sub-phase is a separately-shippable unit. 8h-1 alone unlocks the ability to script-manage routing rules without SQL. 8i-2 alone unlocks Phase 10 starting.

---

## 9. Edge cases

| Scenario | Behavior |
|---|---|
| Tab clicked while another save is in flight | Allow tab switch; in-flight save completes in the background. No special handling. |
| User tries to delete a rule whose source is `label` | Show confirmation modal: "This will be re-created when the container next reports its labels. Remove the `isengard.expose` label on the container to stop it permanently." Allow delete anyway. |
| Adapter test fails | Card shows red "error" pill + the error message inline. Save still allowed (user may be saving to test again). |
| Two browser tabs editing the same rule | Last write wins (existing pattern across the dashboard). The `updated_at` field can be added to PATCH responses for future "you have a stale view" detection. |
| `swap_upstream` called for a hostname with no current entry | Treats as a plain insert: install new_upstream as Active, no drain timer. |
| `swap_upstream` called twice in rapid succession on the same hostname | Second call's drain timer is independent. The first new_upstream becomes the "draining" target of the second swap. Old draining entry from first swap was already replaced by first install — no orphan. |
| Drain timer fires after the swapped-in upstream is also swapped out | The state check in the timer (`if state == Draining`) makes this safe — the new entry is `Active`, drain timer leaves it alone. |
| `grace_period` is `Duration::ZERO` | New upstream replaces old immediately; drain timer fires "after 0s" and finds the entry not in Draining state (because it's the new one). No-op. |

---

## 10. Open questions resolved (record of decisions made in 2026-05-04 brainstorm)

| Question | Decision | Rationale |
|---|---|---|
| Plan C scope split | **One plan for 8h + 8i** | Same approach as Plan A and B; both are small enough |
| Settings page layout | **Convert /settings to tabs (general/networking/notifier)** | Sections will keep accumulating; tabs sort early |
| Adapter API token storage UX | **Plain text in `adapter_config` table; password-masked input in UI** | Pragmatic v1; OS keychain too platform-specific, env-var only is bad UX |
| `swap_upstream` trigger | **Internal Rust API only; no UI/HTTP in Plan C** | Phase 10 wires the orchestration; Plan C just provides the primitive |
| `swap_upstream` key (rule_id vs hostname) | **hostname** | Registry is hostname-keyed; rule_id would add a storage roundtrip for no benefit |

---

## 11. Success criteria

After Plan C ships:

1. ✅ `/settings?tab=networking` renders the Networking tab; Routing rules table populates from the API
2. ✅ User can create a new routing rule via "+ Add rule" → modal → save; new row appears in the table
3. ✅ User can edit an existing UI-source rule by clicking it; PATCH lands; row updates
4. ✅ User can delete a rule; confirmation modal for label-source rules
5. ✅ Adapter cards (cf-tunnel, tailscale, none) render with status pills; API token field is password-masked
6. ✅ Save adapter config persists to `adapter_config` table; "Test connection" button surfaces real success/error
7. ✅ Existing settings (Fleets, Notifier, Enrollment) preserved as `general` and `notifier` tabs; deep-link `/settings?tab=notifier` works
8. ✅ `proxy::swap_upstream(&state, hostname, new_upstream, grace_period)` is callable from agent Rust; existing routing tests still pass
9. ✅ A draining upstream is not selected by `IsengardProxy::upstream_peer` for new connections (verified by unit + integration test)
10. ✅ `cargo build/test/clippy/deny` all clean

---

## 12. Testing strategy

- **Backend HTTP API**: integration tests in `crates/isengard-plugins/dashboard/tests/api_routing.rs` using the existing dashboard test scaffolding (the dashboard plugin tests already have an HTTP test setup pattern — look at any existing `tests/api_*.rs` for shape, or the existing `src/api.rs::tests` mod if inline tests are the pattern). 6-8 tests covering happy path + error cases per endpoint.
- **Frontend components**: no automated component tests — this dashboard plugin doesn't currently have a Vue test runner set up. Manual smoke is acceptable. (Adding one is a separate plan; not in scope here.)
- **Atomic swap**: 3 Rust tests in `crates/isengard-agent/tests/proxy_swap_unit.rs` covering state transitions + drain timing + router skip-behaviour.
- **Manual smoke for the UI**: documented in the PR body. Maintainer verifies the 7 success criteria items 1-7 in a real browser.

---

## 13. Dependencies on other crates / phases

- **Plan A** (Phase 8a-8d) — provides `routing_rules` schema, `RoutingPusher`, `Inventory` storage methods we expose via HTTP
- **Plan B** (Phase 8e-8g) — provides adapter config writes via the same `adapter_config` table; Plan C just adds the UI surface
- **Phase 5 dashboard plugin** — Plan C consumes the existing `axum` router, `useApi` composable, and event stream
- **Existing Vue components** — `<AppShell>`, `<PageHeader>`, `<EmptyState>`, `<StatusPill>`, `<ConfirmDialogShell>` are already in the dashboard
- **`vue-router`** for tab query-param state (already in the Nuxt config)

No phase blocks Plan C from starting. Plan C unblocks Phase 10 (Blue-Green) by providing `swap_upstream`.
