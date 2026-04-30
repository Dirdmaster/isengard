# Phase 5: Dashboard Design Spec

**Status:** Final, 2026-05-01
**Parent spec:** `2026-04-29-platform-pivot-design.md` §9.2 (dashboard plugin)
**Pencil source:** `design/app.pen` — 13 frames covering home variants, hosts table v2, host detail, cmd pane modes
**Author:** AI partner (Opus 4.7), in dialogue with engineer

---

## 1. Why this exists

Phase 4 delivered the platform's nervous system — agents emit events, the controller journals + broadcasts them, the notifier delivers to Telegram/Discord/HTTP. But the user-facing surface is still nothing: zero web UI, no inspection beyond `journalctl` and grep on the journal SQLite. Phase 5 builds the dashboard.

**The dashboard is the product.** It's what your friend opens when they want to know their fleet is healthy. It's what gets screenshot for Twitter/marketing. It's the differentiator — Portainer/Komodo/Coolify all converged on a "list of containers + sidebar nav" pattern that everyone tolerates and no one loves. We're betting on something different: an event-stream-first home + universal cmd pane (navigator + terminal in one) + integrated terminal as part of the chrome.

The friend's deploy window is May 4 — Phase 5 needs to land in 3 days for v1 ship. Scope is everything-shippable, nothing else.

---

## 2. Architecture

### Stack
- **Frontend:** Nuxt 3 + Vue 3 + Tailwind CSS 4 + Pinia + xterm.js
- **Build:** Bun (`bun install` + `bun run build` produces a static SPA in `.output/public/`)
- **Embedding:** `rust-embed` 8 bakes the static bundle bytes into the dashboard plugin's `.rlib` at Rust build time
- **Server:** axum 0.8 — serves the embedded bundle + `/api/v1/*` JSON + `/ws/events` WebSocket
- **Runtime:** Single binary. Zero Node/Bun runtime dependency on deploy. The dashboard plugin spawns its own axum server on a configurable port (default `127.0.0.1:9418`), separate from the controller's gRPC server (`9417`).

### Build pipeline (developer experience)
```
crates/isengard-plugins/dashboard/
├── Cargo.toml              # rust-embed, axum, tokio, isengard-controller, isengard-core
├── build.rs                # if web/dist newer than web/src/**, run `bun install && bun run build`
├── src/
│   ├── lib.rs              # Plugin impl + axum router + WS handler
│   ├── api.rs              # REST endpoint handlers
│   └── ws.rs               # WebSocket → EventBus subscriber
└── web/
    ├── package.json        # bun-managed
    ├── nuxt.config.ts      # static preset, no SSR runtime
    ├── tailwind.config.ts  # Inter + JetBrains Mono + iso-* tokens
    ├── pages/              # Vue routes
    ├── components/         # Vue components
    ├── composables/        # useEvents, useFleet, useHosts, etc.
    └── stores/             # Pinia stores
```

For dev iteration:
- Terminal 1: `cd crates/isengard-plugins/dashboard/web && bun run dev` — Vite HMR on `:3000`, proxies `/api`, `/ws` to the Rust backend
- Terminal 2: `cargo run -- controller` — real backend with real EventBus
- Same workflow as Tauri/Nuxt setups

For CI: `oven-sh/setup-bun@v1` step runs before `cargo build`. Release artifact has zero Node/Bun dependency.

### Runtime model
1. Controller spawns the dashboard plugin via the Phase 4d controller plugin host
2. Dashboard plugin reads `ctx.config` for `bind_addr` (default `127.0.0.1:9418`) and `bus` (downcast from `Arc<dyn Any>` to `EventBus`)
3. Dashboard plugin starts an axum server bound to that addr, serving:
   - `/` (and any client-side route) → embedded `index.html` (SPA fallback)
   - `/_nuxt/*` → embedded JS/CSS chunks
   - `/api/v1/*` → JSON handlers (read-only mostly; some POST for actions)
   - `/ws/events` → WebSocket subscribed to controller's EventBus
4. On controller shutdown, dashboard plugin's axum server gracefully shuts down via the plugin lifecycle's `stop()` method

### Why this stack (and not alternatives)

| Option | Why not |
|---|---|
| Leptos / Yew (Rust → WASM) | Bundle size + cold-start tax (500KB-1.5MB WASM). DX cost (5-10s rebuild iteration vs Vue's 100ms HMR). No Tailwind autocomplete in Rust strings. Component ecosystem is thin. The user already knows Vue. Rust-in-browser isn't a virtue here. |
| Plain Rust SSR + handrolled HTML/JS | Smallest bundle but every interaction is hand-coded. Dies quickly past the home view. |
| React (Next.js) instead of Vue (Nuxt) | Both work. Vue is what the user knows from keyz. Lower friction. |
| Server-side rendering (Nuxt SSR mode) | Requires a Node runtime, breaks the single-binary promise. Static preset gets us SPA + embedded delivery + same SEO/perf properties for an admin tool. |

---

## 3. Visual identity (design tokens)

All tokens live as Pencil variables in `design/app.pen` AND mirror as Tailwind theme tokens in `web/tailwind.config.ts`. Source of truth: the Pencil file.

### Color palette

| Token | Hex | Use |
|---|---|---|
| `iso-bg-base` | `#0b0d0f` | Page background |
| `iso-bg-elevated` | `#0e1114` | Cards, inspector, drawer |
| `iso-bg-overlay` | `#15181b` | Pills, kbd badges, hover panels |
| `iso-bg-row-hover` | `#11151a` | Row hover state |
| `iso-bg-selected` | `#0f1a12` | Selected row tint (subtle green wash) |
| `iso-border-subtle` | `#1c2024` | Hairlines, dividers |
| `iso-border-strong` | `#2a2f35` | Active borders, pressed states |
| `iso-text-primary` | `#e6e8eb` | Headlines, host names |
| `iso-text-secondary` | `#d8dde2` | Body text |
| `iso-text-muted` | `#8a9099` | Meta text, timestamps |
| `iso-text-faint` | `#6f7680` | Section labels, hint text |
| `iso-accent-success` | `#4ade80` | Healthy state, UPDATED events |
| `iso-accent-success-soft` | `#1e3826` | Subtle green backgrounds |
| `iso-accent-warn` | `#fbbf24` | Updating state, PULLING events |
| `iso-accent-error` | `#f87171` | Failed state, FAILED events |
| `iso-accent-info` | `#c084fc` | Disconnected state, agent.disconnect_long events |
| `iso-accent-neutral` | `#94a3b8` | CHECKED events, neutral keywords |
| `terminal-bg` | `#050505` | Shell/log content background (darker than page bg for contrast) |

Status colors map:
- **Green** → healthy / up_to_date / connected / success
- **Amber** → updating / pulling / older agent version
- **Red** → failed / error
- **Purple/Info** → disconnected / agent.disconnect_long / unknown

### Typography
- **`iso-font-sans`** → Inter (system fallback). Used for all UI chrome, body copy, labels.
- **`iso-font-mono`** → JetBrains Mono (system fallback). Used for: timestamps, container/image names, log lines, kbd shortcuts, digests.

### Type scale
- `iso-text-xs` 11pt — meta, kbd hints, timestamps
- `iso-text-sm` 12pt — UI labels, secondary text
- `iso-text-base` 13pt — body text, table cells
- `iso-text-md` 14pt — top bar, sparkline glyphs
- `iso-text-lg` 16pt — page headers, host names in cards

### Spacing scale
4 / 8 / 12 / 16 / 20 / 24 px (`iso-space-1` through `iso-space-6`).

### Radii
- `iso-radius-sm` 4 — small chips, buttons
- `iso-radius-md` 6 — cards, dropdowns
- `iso-radius-lg` 8 — major panels (state cards, terminal panel)
- `iso-radius-pill` 999 — fully-rounded badges

### Iconography
**Lucide icons via icon font.** No standalone Unicode glyphs as text — they fall back unreliably across fonts (we hit this in design with `▾`, `◎`, `⚙`, `✓`, `!` all needing replacement). Standard set:
- `chevron-down`, `chevron-right`, `chevron-up` — disclosure
- `terminal` — shell/cmd pane
- `compass` — Atlas mode toggle (concept retained as nav button even though Atlas itself is dropped)
- `settings` — settings cog
- `check` — healthy / success states
- `triangle-alert` — warning / amber states
- `x` — close
- `plus` — add / create
- `search` — search / cmd input
- `square`, `panel-bottom` — position toggle (center / dock)
- `layers` — stack icon
- `package` — service/container icon
- `server` — host icon
- `zap` — force-action / quick action
- `arrow-down`, `arrow-up` — direction indicators
- `ellipsis` — overflow menu
- `sliders-horizontal` — filter

---

## 4. Information architecture

### Top bar (chrome, persistent)

```
[● isengard · All fleets ▾]  [Home]  Hosts  Stacks  Events     [◎ Atlas]  [Search or jump…  ⌘K]  [⚙]
```

| Slot | What | Behavior |
|---|---|---|
| Brand cluster (left) | Green health dot + "isengard" + fleet picker | Fleet picker opens dropdown: All fleets / prod / staging / edge-pi / + Add fleet. Selection cross-cuts everything. |
| Tab bar (center) | Home / Hosts / Stacks / Events | Active tab styled with `bg-overlay`. Inactive muted. Click navigates. |
| Atlas toggle (right cluster) | Decorative for v1 (Atlas dropped) but reserved as a button | When pressed in v1, no-ops or shows "Atlas mode coming in v1.x" toast |
| ⌘K search | Opens cmd pane | See §9 |
| Settings cog | Opens Settings page | Navigates to `/settings` |

### Cmd pane (universal, summoned)

Two states (mode), two positions:

| Mode | Position | Trigger |
|---|---|---|
| **Navigator** (default open) | **Center** floating, dimmed backdrop | `⌘K` from anywhere |
| **Navigator** | **Docked bottom** | Toggle from center via `⌘.` |
| **Terminal** | **Center** floating | Pick "Open shell on X" from navigator |
| **Terminal** | **Docked bottom** | `⌘.` from center, OR open second shell |

The cmd pane is the universal entry point. It can navigate the app (`prod-01` → host detail), execute actions (`force update web`), open shells (`Open shell on web @ prod-01`), and run AI queries (deferred to v1.x). Same surface for everything.

### Bottom area (persistent chrome)

When cmd pane is centered or closed: **status bar** (40px) with `live · N events today · controller v0.1.0-alpha · keyboard hints`.

When cmd pane is docked at bottom: **the cmd pane absorbs the bottom area** (320-360px tall). Main grid auto-shrinks. Status info absorbed into cmd pane footer.

---

## 5. Routes / pages

| Path | Page | Pencil frame |
|---|---|---|
| `/` | Home (event timeline + state strip + inspector) | `Home · Timeline (v1 — locked)` |
| `/hosts` | Hosts table | `Hosts tab v2 (enhanced)` |
| `/hosts/:id` | Host Detail (cards with stacks) | `Host Detail · staging` |
| `/stacks` | Stacks table (cross-fleet) | not mocked — adapts Hosts pattern |
| `/stacks/:id` | Stack Detail (services, events, history) | not mocked — adapts Host Detail pattern |
| `/events` | Event journal (full, filterable) | not mocked — adapts Home timeline |
| `/events/:id` | Event detail (rare deep-link) | not mocked — adapts inspector |
| `/settings` | Settings (fleets, channels, agent enrollment) | not mocked — conventional form patterns |

All routes share the top bar + cmd pane chrome.

---

## 6. Component inventory

Each component below has: **Intent** (what it's for), **Pencil reference** (frame name in `app.pen`), **Variants** (states/modes), **Props sketch** (Vue interface).

### `<TopBar />`
- **Intent:** persistent top chrome with brand, fleet picker, tabs, cmd pane trigger, settings
- **Pencil:** `Top bar` (inside any home frame)
- **Variants:** active tab (Home/Hosts/Stacks/Events)
- **Props:** `:activeTab`, `:fleetCount`, `@fleet-change`

### `<FleetPicker />`
- **Intent:** dropdown to select active fleet scope (or "All fleets")
- **Pencil:** `Fleet picker` (inside top bar's brand cluster)
- **Variants:** open / closed; option states
- **Props:** `:current`, `:fleets`, `@select`

### `<StateStrip />` (Home view)
- **Intent:** at-a-glance fleet health summary, exception-driven (calm when healthy, loud when broken)
- **Pencil:** `State card · prod` and `State card · staging` (inside `Home · Timeline`)
- **Variants:** healthy (compact, green check, single line), with-issues (expanded with issue rows)
- **Composition:**
  - Card frame (bg-elevated, padding, border, radius)
  - Header row: status icon (triangle-alert amber / check green / x red) + "Fleet · {name}" + colored stats line + sub line + mini-grid (one cell per service)
  - "N NEED ATTENTION" label (only when issues > 0)
  - Issue rows: glow dot + "{service} on {host}" + detail
- **Props:** `:fleet`, `:issues`, `:totalServices`, `:lastCycleSecs`

### `<EventRow />`
- **Intent:** one event in the timeline
- **Pencil:** `Event · UPDATED web (selected)` and siblings (inside `Home · Timeline`)
- **Variants:** selected (green left border + tinted bg), default (transparent), hover
- **Composition:** row frame + timestamp (mono) + kind keyword (mono color-coded) + body text + host name
- **Kind colors:** UPDATED green, FAILED red, CHECKED neutral, PULLING amber, DISCONNECT info
- **Props:** `:event`, `:selected`, `@click`, `@dblclick`

### `<DayLabel />`
- **Intent:** group events by day in the timeline
- **Pencil:** `Day label · Today` etc.
- **Variants:** Today / Yesterday / "April 28" (date format)
- **Composition:** small caps text in `text-faint`

### `<Inspector />` (variants: event detail, host detail)
- **Intent:** right-side context pane for selected entity (HOME/EVENTS only — list pages use full-page detail)
- **Pencil:** event variant in `Inspector column` of `Home · Timeline`; host variant in `Inspector column` of `Hosts tab v2` (now removed; preserved as reference)
- **Composition:** SECTION LABEL + entity header (icon + name + meta) + KV block + sub-list + Quick Actions
- **Props:** `:entity`, `:type` (event | host)

### `<Sparkline />`
- **Intent:** mini bar chart of activity over time, single-line
- **Pencil:** in `Hosts tab v2` ACTIVITY column (text-based using ▁▂▃▄▅▆▇█ block glyphs)
- **Variants:** state colors (success/warn/error/info)
- **Composition:** monospace text rendering N bar glyphs
- **Note for impl:** Use `<svg>` with `<rect>` bars for production (better control + animation). The text-glyph approach was for design fidelity in Pencil only.
- **Props:** `:data` (array of nums 0-1), `:color`, `:width`

### `<StatusPill />`
- **Intent:** compact status indicator with text
- **Pencil:** "All fleets ▾", filter chips, etc.
- **Variants:** state (success/warn/error/info/neutral); size (xs/sm)
- **Composition:** rounded frame with icon + text
- **Props:** `:state`, `:label`, `:size`

### `<HostRow />` (Hosts tab v2)
- **Intent:** one host in the Hosts table
- **Pencil:** `Row · prod-01` etc. (inside `Hosts tab v2`)
- **Variants:** default / hovered (actions visible) / selected (green left border)
- **Composition:** row frame with cells in fixed widths:
  - HOST 170w: status dot + hostname
  - FLEET 70w: fleet name
  - ACTIVITY 130w: sparkline
  - STACKS 80w: "N · M svcs"
  - LATEST 600w: latest event mono color-coded
  - LAST SEEN 90w: relative time
  - AGENT 60w: version (amber if older than fleet)
  - HOVER ACTIONS (right-aligned, fade in on hover): `[zap]` `[terminal]` `[ellipsis]`
- **Props:** `:host`, `:selected`, `:hovered`, `@click`, `@action`

### `<FleetWeather />`
- **Intent:** page-level health-over-time strip on Hosts tab
- **Pencil:** `Fleet weather` (top of `Hosts tab v2`)
- **Composition:** label + wide sparkline + summary text + range selector
- **Props:** `:events` (last 24h aggregated), `:range`, `@range-change`

### `<HostCard />` (Host Detail page)
- **Intent:** rich card for a single host showing its stacks
- **Pencil:** `Host card · prod-01` etc. (inside `Host Detail · staging`)
- **Variants:** healthy (collapsed-default, single "All N stacks healthy" line), with-issues (expanded showing problem stacks)
- **Composition:** card frame + header (host info + stats summary + agent meta) + stacks list
- **Props:** `:host`, `:stacks`, `:expanded`, `@toggle`

### `<StackRow />` (inside HostCard)
- **Intent:** one stack within a host's card
- **Pencil:** `Stack · wordpress` etc.
- **Composition:** row frame + layers icon + stack name + service count meta + service chips (right-aligned)
- **Props:** `:stack`, `:services`, `@click`

### `<ServiceChip />`
- **Intent:** one service inside a stack
- **Pencil:** chips inside `Stack · wordpress`
- **Composition:** small pill with service name + colored status dot
- **Props:** `:service`, `:state`

### `<CmdPane />`
- **Intent:** the universal navigator+terminal surface
- **Pencil:** `Cmd panel` in:
  - `Home · Cmd pane (navigator + terminal)` — center mode, navigator
  - `Home · Cmd pane (terminal mode)` — center mode, terminal
  - `Home · Cmd pane (docked & integrated)` — docked mode, terminal
- **Variants:**
  - Position: `center` (640w × 520h, dimmed backdrop) | `docked` (full-width × 320h, no backdrop, flush edges)
  - Mode: `navigator` (search + results) | `terminal` (xterm content)
- **Sub-components:**
  - `<CmdInput />` — search/command input with cursor + esc kbd
  - `<CmdResult />` — one navigator result row (icon + name + meta + kbd hint)
  - `<CmdSection />` — section label (HOSTS / CONTAINERS / ACTIONS)
  - `<CmdTerminal />` — xterm.js mount with breadcrumb header + footer keys
  - `<CmdBreadcrumb />` — terminal mode header showing app context
- **Props:** `:open`, `:position`, `:mode`, `@close`, `@toggle-position`, `@toggle-mode`

### Bottom drawer states (alternative explored, NOT chosen for v1)
- `Home · Drawer collapsed` and `Home · Drawer expanded` exist in Pencil as the abandoned VS-Code-tabbed approach. Reference only — not implemented.

### Bottom-snap floating (alternative explored, NOT chosen)
- `Home · Cmd pane (snap to bottom)` exists in Pencil as the floating-bottom popover approach. Reference only — replaced by integrated docked.

---

## 7. Data model additions

### New entity: `Stack`

```sql
CREATE TABLE stacks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB NOT NULL REFERENCES hosts(id),
    name            TEXT NOT NULL,
    source          TEXT NOT NULL CHECK(source IN ('compose', 'manual', 'inferred')),
    discovered_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(host_id, name)
);
CREATE INDEX idx_stacks_host_id ON stacks(host_id);
```

### Field on existing `hosts` table: `fleet`

```sql
ALTER TABLE hosts ADD COLUMN fleet TEXT NOT NULL DEFAULT 'default';
CREATE INDEX idx_hosts_fleet ON hosts(fleet);
```

Fleets are user-defined tags. Hosts can be moved between fleets by edit. v1 has a single "default" fleet auto-created; user creates more via Settings.

### Service-to-stack mapping

Containers (services) get associated with stacks via:
1. **Compose label**: `com.docker.compose.project=<name>` → stack name (Docker Compose's standard)
2. **Override label**: `isengard.stack=<name>` → explicit stack assignment
3. **Fallback**: container without either label → single-service stack named after the container

The agent does this discovery on each cycle and includes stack info in the heartbeat or inventory snapshot. Controller persists the mapping.

### Migration sequence
1. `0003_stacks.sql` — creates stacks table
2. `0004_hosts_fleet.sql` — adds fleet column with default
3. Existing hosts get `fleet = 'default'` automatically
4. Existing containers get a single-service stack named after them (initial sync)

Plus core changes:
- `isengard-storage::Stack` struct + `StackId` newtype (or just `i64`)
- `Inventory::insert_stack`, `list_stacks(host_id?)`, `set_host_fleet`
- Event variants don't need to change — events already have `container_name` and `host_id` fields; we add a stack reference if needed in v1.x

---

## 8. API surface

All endpoints under `/api/v1/`. JSON in/out. CORS allows the dev server origin (`localhost:3000`) when in debug build.

### REST

#### Hosts
- `GET /api/v1/hosts?fleet=<tag>&state=<healthy|warn|error|disconnected>` — list with optional filters
- `GET /api/v1/hosts/:id` — single host with stacks list inline
- `POST /api/v1/hosts/enroll` — request body `{ fleet?, hostname?, fingerprint }` → returns `{ agent_id, enrollment_token, install_command }`
- `PATCH /api/v1/hosts/:id` — update fleet or other mutable fields
- `DELETE /api/v1/hosts/:id` — decommission (revokes token, removes from inventory)
- `GET /api/v1/hosts/:id/events?limit=N&before=<ts>` — event history for one host
- `GET /api/v1/hosts/:id/sparkline?range=24h` — pre-aggregated bar data for the row sparkline
- `POST /api/v1/hosts/:id/actions/force-update` — trigger update cycle now

#### Stacks
- `GET /api/v1/stacks?fleet=<tag>&host_id=<id>&state=<...>` — list, sortable
- `GET /api/v1/stacks/:id` — single stack with services
- `POST /api/v1/stacks/:id/actions/force-update` — update all services in stack atomically

#### Services (containers)
- `GET /api/v1/services?stack_id=<id>` — services in a stack
- `GET /api/v1/services/:id` — single service detail

#### Events
- `GET /api/v1/events?limit=N&kind=<filter>&host_id=<id>&stack_id=<id>&since=<ts>` — paginated journal query
- `GET /api/v1/events/:id` — single event

#### Fleets
- `GET /api/v1/fleets` — list all fleet tags with host counts
- `POST /api/v1/fleets` — create fleet (just registers a tag; no entity)
- `DELETE /api/v1/fleets/:tag` — only allowed if no hosts assigned

#### Settings
- `GET /api/v1/settings` — current notifier config, agent config, etc.
- `PATCH /api/v1/settings` — update fields

#### Live shell / log streaming
- `GET /api/v1/services/:id/logs?follow=true` — Server-Sent Events stream of stdout/stderr (alternative: WebSocket)
- `WS /api/v1/services/:id/shell` — bidirectional WebSocket for `docker exec -it`

### WebSocket: `/ws/events`

Per-client subscription to the controller's EventBus. On connect, server immediately sends a `welcome` frame with current fleet snapshot, then streams events as they arrive.

Frame format (JSON):
```json
{
  "type": "event" | "welcome" | "ping" | "error",
  "event": { /* full Event payload, same shape as REST */ }
}
```

Client sends `{ "type": "ping" }` every 30s to keep connection alive. Server responds `{ "type": "pong" }`. Connection-level keep-alive prevents idle disconnects on intermediate proxies.

---

## 9. Cmd pane behavior

### Trigger
- Global: `⌘K` (macOS) / `Ctrl+K` (linux/windows). Opens center-mode navigator.
- From URL anywhere in the app — works on any route.

### Modes

#### Navigator mode (default)
- Empty input → shows recent + pinned items (e.g. last 3 hosts viewed, common actions)
- Typing → fuzzy match across:
  - Hosts (by name, hostname, fingerprint)
  - Stacks (by name)
  - Services (by name, image)
  - Events (by kind, summary)
  - Actions (verb-prefixed, e.g. `force update X`, `restart X`)
- Results categorized in sections (HOSTS / STACKS / CONTAINERS / EVENTS / ACTIONS)
- Each result has icon + name + meta + optional kbd hint
- `↑↓` to move, `Enter` to select, `Esc` to close
- Special prefixes:
  - `:` — actions only (e.g. `:nav events --kind failed`)
  - `$` — shell command (after picking a container context)
  - `?` — help / examples
- Selecting a navigation result navigates the app behind the pane (pane stays open OR closes depending on behavior — see below)

#### Terminal mode
- Triggered by selecting "Open shell on X" or "Tail logs on X" from navigator
- Pane content swaps from results list to xterm.js terminal
- Header: terminal icon + breadcrumb (`isengard › prod › web @ prod-01 › shell`) + connected indicator + position toggle + close
- Bottom: prompt with cursor for shell input
- Footer: `⌘P navigator · ⌘N new shell · ⌘W close · ⌘↑ scrollback · ⌘. toggle position`
- Shell connects via WS to `/api/v1/services/:id/shell`
- Logs (read-only) connects via SSE to `/api/v1/services/:id/logs?follow=true`

### Positions

#### Center (default)
- 640w × 520h, centered horizontally + vertically
- Dimmed backdrop (`#000000a0`)
- Click outside to close
- Used for transient navigation / quick action

#### Docked bottom
- Full-width (with 16px side margins) × 320-360h
- No backdrop dim — dashboard above stays interactive
- Flush edges (no shadow), border-top subtle
- Main grid above auto-shrinks to make room (cmd pane is part of layout, not overlay)
- Used for persistent terminal sessions / monitoring

Toggle via `⌘.` or position-toggle in pane header.

### Multiple sessions
- One open cmd pane at a time in v1
- `⌘N` from terminal mode opens a NEW cmd pane (replaces current with fresh navigator); previous shell session continues running in background, accessible via navigator search "open shell" or recent list
- v1.x: tab strip inside docked pane for multiple visible sessions

### Keyboard shortcuts (full reference)

Global:
- `⌘K` — open cmd pane (navigator)
- `⌘.` — toggle position (center ↔ docked) on open pane
- `j/k` — move down/up in lists (when not in input)
- `/` — focus filter on current page (e.g. Hosts filter chip)
- `?` — show keyboard hints overlay

In cmd pane navigator:
- `↑↓` — move selection
- `Enter` — select
- `Esc` — close
- `⌘P` — switch from terminal back to navigator (when in terminal mode)

In cmd pane terminal:
- `⌘P` — back to navigator
- `⌘N` — new shell session
- `⌘W` — close pane
- `⌘↑` — scrollback
- `⌘.` — toggle position

---

## 10. Real-time data flow

### Event flow (read path)

1. Agent emits event via `OutboundEmitter` (Phase 4b)
2. Event arrives at controller's gRPC Sync handler
3. Handler calls `persist_and_broadcast(journal, bus, event)` — writes to SQLite then broadcasts
4. Dashboard plugin's WebSocket handler subscribes to `bus.subscribe()` on connect
5. Each WS connection receives a `tokio::sync::broadcast::Receiver<Event>`
6. Handler loops `rx.recv().await`, serializes to JSON, sends WS frame
7. Frontend Pinia store (`useEventsStore`) listens to WS, prepends to events array
8. Reactive Vue components re-render

### State sync (cold load)

1. Page mounts → composable issues REST requests
2. e.g. Home view: `GET /api/v1/events?limit=50` + `GET /api/v1/fleets`
3. Pinia stores hydrate
4. WebSocket connects, server sends `welcome` frame with current snapshot
5. Subsequent updates flow via WS

### Action flow (write path)

1. User clicks "Force update web on prod-01"
2. Client POSTs to `/api/v1/services/<id>/actions/force-update`
3. Controller forwards to agent via gRPC (next sync cycle includes a force-update directive)
4. Agent processes, emits events as it works (`update.checked`, `update.success`/`update.failed`)
5. Events flow back through the read path → UI updates reactively

---

## 11. Authentication

### v1 (this phase)
- **No authentication.** Dashboard binds to `127.0.0.1:9418` by default.
- If user wants external access, they put it behind their existing reverse proxy with auth (Caddy + auth_form, Authelia, Tailscale Funnel, etc.)
- Documented in README + a deploy guide

### v1.x (deferred)
- Bearer token (`ISENGARD_DASHBOARD_TOKEN` env var, similar to `ISENGARD_TOKEN` for the gRPC controller)
- OIDC / SSO integration (auth_request to upstream provider)
- Per-user role-based access (admin vs read-only)

### v2 (much later)
- Hosted SaaS uses account-based auth with per-account isolation

---

## 12. Out of scope

Explicitly NOT in Phase 5:

- **Atlas spatial fleet view** — dropped after design analysis. State strip + cmd pane already cover the at-a-glance need; Atlas would duplicate. Asset preserved at `weavers-vault/.superpowers/brainstorm/93385-1777487414/content/atlas-home.html` for marketing site hero.
- **AI chat in cmd pane** — `?` prefix reserved as future hook; v1 just shows help. v1.x lands LLM integration with journal context.
- **Multi-host stacks (Swarm-style replication)** — single host per stack in v1.
- **Mobile breakpoint** — desktop-first for v1. Phone view in v1.x.
- **Bulk actions** (multi-select rows) — single-action only in v1.
- **Custom dashboards / widget system** — fixed layout in v1.
- **Service uptime / response-time graphs** — we don't collect this telemetry. v1.x with `metrics` plugin.
- **Healthcheck-driven rollback** — owner of update lifecycle is updater, not dashboard.
- **Pre/post-update hooks** — `hooks` plugin in v1.x.
- **Image registry browsing** — out of scope; Isengard observes registries via existing `RegistryClient`, doesn't expose them as a UI surface.
- **In-app log search across containers** — single-container log streaming yes; cross-container search no.
- **Audit log of user actions** (who clicked what when) — v1.x with `dashboard.action_taken` events to journal.

---

## 13. Sub-phase breakdown

| Sub-phase | Scope | Plan file |
|---|---|---|
| **5a** | Nuxt scaffold + Tailwind + bundle pipeline + axum static-serving | `2026-05-01-phase-5a-dashboard-scaffold.md` |
| **5b** | REST API endpoints + WebSocket `/ws/events` + Pinia stores | `2026-05-01-phase-5b-api-and-ws.md` |
| **5c** | Home view (state strip + event timeline + inspector + cmd pane navigator) | `2026-05-01-phase-5c-home-view.md` |
| **5d** | Hosts table + Host Detail + Stacks table + Stack Detail | `2026-05-01-phase-5d-hosts-stacks.md` |
| **5e** | Events tab + Settings + Add host modal + cmd pane terminal mode | `2026-05-01-phase-5e-events-settings-shell.md` |

Each sub-phase is independently shippable. After 5a the bundle pipeline works (just shows a placeholder page). After 5b the API contract is testable (curlable). 5c through 5e build the actual UI in incremental layers.

---

## 14. Done criteria for Phase 5

1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes (no regressions on Phases 0-4)
3. `just ci-local` clean (cargo-deny mandatory)
4. `bun --cwd crates/isengard-plugins/dashboard/web run build` produces `<300KB` gzipped JS bundle (target; track in CI)
5. End-to-end smoke: run controller + agent + a labelled container with stale tag → open dashboard at `http://localhost:9418` → see the home view populated → see the update happen via the event timeline in real time
6. Tag `v0.1.0-alpha.phase5-complete`
7. Friend's deploy on May 4 uses the dashboard

---

## 15. Visual fidelity reference

The Pencil source (`design/app.pen`) contains 13 frames at the time of this spec:

1. `Home · Timeline (v1 — locked)` — canonical home view
2. `Home · Drawer collapsed` — abandoned VS-Code drawer pattern (reference only)
3. `Home · Drawer expanded` — abandoned VS-Code drawer pattern (reference only)
4. `Home · Cmd pane (navigator + terminal)` — center mode, navigator showing "prod" search
5. `Home · Cmd pane (terminal mode)` — center mode, transformed into shell session
6. `Home · Cmd pane (snap to bottom)` — abandoned floating-popover bottom-snap (reference only)
7. `Home · Cmd pane (docked & integrated)` — chosen docked-bottom-integrated model
8. `Hosts tab` — abandoned simple flat table (reference only — superseded by v2)
9. `Host Detail · staging` — host detail page with cards-with-stacks layout
10. `Hosts tab v2 (enhanced)` — chosen Hosts table with sparklines + inline latest event + hover actions + fleet weather strip + add-host button + keyboard hints, full-width (no inspector)

Plus reusable variables (35 design tokens) declared at the document level.

---

## End of spec

Implementation plans for sub-phases 5a-5e are at `docs/superpowers/plans/2026-05-01-phase-5{a,b,c,d,e}-*.md`.
