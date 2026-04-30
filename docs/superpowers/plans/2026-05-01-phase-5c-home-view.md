# Phase 5c: Home View — State Strip + Timeline + Cmd Pane Navigator

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Build the actual Home view that's been designed in Pencil. Top bar with fleet picker + tabs + ⌘K + settings. State strip cards (per fleet, exception-driven). Event timeline (live, color-coded, click for inspector). Inspector pane (right column, event detail). Cmd pane (center modal, navigator mode — terminal lands in 5e). End state: open `http://localhost:9418/` and the home view matches the locked Pencil design `Home · Timeline (v1 — locked)`, populated with live data.

**Architecture:** Vue components wrap Pinia stores from 5b. Components mirror the structure in `app.pen`'s `Home · Timeline (v1 — locked)` frame: TopBar / StateStrip / EventTimeline / Inspector / BottomStatusBar. Cmd pane is its own overlay that summons via global ⌘K shortcut and renders centered with backdrop. All Tailwind classes use `iso-*` tokens from 5a's config.

**Tech stack additions:** `@vueuse/core` for keyboard shortcuts and useElementSize. `fuse.js` for fuzzy navigator search.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-05-01-phase-5-dashboard-design.md` §6 (component inventory) + §9 (cmd pane).

---

## Scope

**In:**
- Add `@vueuse/core` and `fuse.js` to dashboard `package.json`
- New components in `dashboard/web/components/`:
  - `TopBar.vue` — brand + fleet picker + tabs + atlas button + ⌘K trigger + settings cog
  - `FleetPicker.vue` — dropdown
  - `StateStrip.vue` — fleet card with exception-driven issue rows + mini-grid + colored stats
  - `EventTimeline.vue` — vertical list of day-grouped events
  - `EventRow.vue` — single event row, color-coded, selectable
  - `DayLabel.vue` — section header
  - `Inspector.vue` — right pane (event detail variant)
  - `BottomStatusBar.vue` — live indicator + version + keyboard hints
  - `CmdPane.vue` — overlay modal (navigator mode only in 5c; terminal in 5e)
  - `CmdInput.vue`, `CmdResultRow.vue`, `CmdSection.vue` — sub-components of CmdPane
- New page: `pages/index.vue` — assembles TopBar + 2-col grid (timeline + inspector) + BottomStatusBar + CmdPane overlay
- Selection state lifted to a Pinia store: `useUiStore` (selected event id, cmd pane open state, fleet filter)
- Global keyboard handler: ⌘K opens cmd pane, Esc closes, Cmd+. toggles position (center/dock — center only in 5c)
- Cmd pane navigator: searches across hosts + events + actions using fuse.js. Empty state shows recent items + tips.
- Inspector reactive to selected event (from useUiStore); shows event detail with kv block + digest before/after + quick action buttons (placeholder onclick handlers)
- StateStrip reads from useFleetsStore + useEventsStore — issue rows derived from events with kind matching `update.failed` / `update.failed` / `agent.disconnect_long` in last N minutes (a derived computed)

**Out (deferred to 5d / 5e):**
- Atlas mode toggle action (button shows but no-op for now)
- Settings cog action (no-op; lands when settings page exists in 5e)
- Cmd pane terminal mode (5e — needs xterm.js + WS shell endpoint)
- Cmd pane bottom-snap position (5e — together with terminal)
- Real action button wiring (currently the inspector "Open shell" button is a no-op; lands when shell is wired in 5e)
- Tab navigation actually working (Hosts/Stacks/Events tab clicks navigate but those routes don't exist yet — placeholder pages in 5d/5e)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes (no regressions)
3. `bun --cwd crates/isengard-plugins/dashboard/web run build` succeeds
4. Manual smoke: with controller running + at least 1 host + a few events in the journal, opening `http://localhost:9418/` shows:
   - Top bar with brand + "All fleets" picker + active "Home" tab + atlas button + ⌘K + settings cog
   - State strip card(s) — one per fleet, with mini-grid of services and exception rows when there are issues
   - Event timeline showing recent events grouped by day, color-coded by kind
   - Click an event → inspector populates on the right with event detail
   - Press ⌘K → cmd pane opens centered with dimmed backdrop, search input focused
   - Type in cmd pane → results filter live (hosts, events, actions)
   - Esc → cmd pane closes
5. Tag `v0.1.0-alpha.phase5c` set locally
6. **Not pushed**

---

## File Structure

```
crates/isengard-plugins/dashboard/web/
├── package.json                            # MODIFY: + @vueuse/core, fuse.js
├── stores/
│   └── ui.ts                               # NEW: useUiStore (selected event, cmd pane state, fleet filter)
├── composables/
│   └── useShortcuts.ts                     # NEW: global keyboard wiring
├── components/
│   ├── TopBar.vue                          # NEW
│   ├── FleetPicker.vue                     # NEW
│   ├── StateStrip.vue                      # NEW
│   ├── EventTimeline.vue                   # NEW
│   ├── EventRow.vue                        # NEW
│   ├── DayLabel.vue                        # NEW
│   ├── Inspector.vue                       # NEW
│   ├── BottomStatusBar.vue                 # NEW
│   ├── CmdPane.vue                         # NEW
│   ├── CmdInput.vue                        # NEW
│   ├── CmdResultRow.vue                    # NEW
│   └── CmdSection.vue                      # NEW
├── pages/
│   └── index.vue                           # MODIFY: assemble all components
└── app.vue                                 # MODIFY: mount global shortcuts handler
```

---

## Task 1: Add deps + UI store + keyboard shortcuts composable

**Files:**
- Modify: `web/package.json`
- Create: `web/stores/ui.ts`
- Create: `web/composables/useShortcuts.ts`
- Modify: `web/app.vue`

- [ ] **Step 1: Add deps**

In `web/package.json`, append to `dependencies`:

```json
"@vueuse/core": "^11.2.0",
"fuse.js": "^7.0.0"
```

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun install
```

- [ ] **Step 2: Create stores/ui.ts**

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'

export type CmdPanePosition = 'center' | 'dock'
export type CmdPaneMode = 'navigator' | 'terminal'

export const useUiStore = defineStore('ui', () => {
  const selectedEventId = ref<number | null>(null)
  const cmdPaneOpen = ref(false)
  const cmdPanePosition = ref<CmdPanePosition>('center')
  const cmdPaneMode = ref<CmdPaneMode>('navigator')
  const activeFleet = ref<string>('all')

  function selectEvent(id: number | null) {
    selectedEventId.value = id
  }

  function openCmdPane(mode: CmdPaneMode = 'navigator') {
    cmdPaneMode.value = mode
    cmdPaneOpen.value = true
  }

  function closeCmdPane() {
    cmdPaneOpen.value = false
  }

  function toggleCmdPanePosition() {
    cmdPanePosition.value = cmdPanePosition.value === 'center' ? 'dock' : 'center'
  }

  function setActiveFleet(name: string) {
    activeFleet.value = name
  }

  return {
    selectedEventId,
    cmdPaneOpen,
    cmdPanePosition,
    cmdPaneMode,
    activeFleet,
    selectEvent,
    openCmdPane,
    closeCmdPane,
    toggleCmdPanePosition,
    setActiveFleet,
  }
})
```

- [ ] **Step 3: Create composables/useShortcuts.ts**

```typescript
import { useEventListener } from '@vueuse/core'

export function useShortcuts() {
  const ui = useUiStore()

  useEventListener(window, 'keydown', (e: KeyboardEvent) => {
    const meta = e.metaKey || e.ctrlKey
    if (meta && e.key === 'k') {
      e.preventDefault()
      ui.openCmdPane('navigator')
      return
    }
    if (meta && e.key === '.') {
      e.preventDefault()
      ui.toggleCmdPanePosition()
      return
    }
    if (e.key === 'Escape' && ui.cmdPaneOpen) {
      ui.closeCmdPane()
    }
  })
}
```

- [ ] **Step 4: Wire shortcuts in app.vue**

```vue
<template>
  <div class="min-h-screen bg-iso-bg-base text-iso-text-primary font-sans antialiased">
    <NuxtPage />
  </div>
</template>

<script setup lang="ts">
useShortcuts()
</script>
```

- [ ] **Step 5: Build the bundle**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
```

Expected: success.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): UI store + global keyboard shortcuts (⌘K, ⌘., Esc)"
```

---

## Task 2: TopBar + FleetPicker + BottomStatusBar

**Files:**
- Create: `web/components/TopBar.vue`
- Create: `web/components/FleetPicker.vue`
- Create: `web/components/BottomStatusBar.vue`

- [ ] **Step 1: TopBar.vue**

```vue
<template>
  <div class="h-13 border-b border-iso-border-subtle px-5 flex items-center gap-4 text-iso-sm">
    <!-- Brand cluster -->
    <div class="flex items-center gap-2">
      <div class="w-2.5 h-2.5 rounded-full bg-iso-success"></div>
      <span class="font-semibold text-iso-text-primary tracking-tight">isengard</span>
      <span class="text-iso-text-faint">·</span>
      <FleetPicker />
    </div>

    <!-- Tab bar -->
    <nav class="flex items-center gap-0.5 ml-4">
      <NuxtLink
        v-for="tab in tabs"
        :key="tab.path"
        :to="tab.path"
        class="px-3 py-1.5 rounded-iso-sm text-iso-sm transition-colors"
        :class="$route.path === tab.path ? 'bg-iso-bg-overlay text-iso-text-primary font-medium' : 'text-iso-text-muted hover:text-iso-text-secondary'"
      >
        {{ tab.label }}
      </NuxtLink>
    </nav>

    <div class="flex-1"></div>

    <!-- Right cluster -->
    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-text-secondary text-iso-sm flex items-center gap-1.5 hover:border-iso-border-strong"
      title="Atlas mode (coming in v1.x)"
    >
      <Icon name="lucide:compass" class="w-3.5 h-3.5 text-iso-text-muted" />
      Atlas
    </button>

    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm flex items-center gap-2 hover:border-iso-border-strong"
      @click="ui.openCmdPane('navigator')"
    >
      <Icon name="lucide:search" class="w-3.5 h-3.5 text-iso-text-muted" />
      <span class="text-iso-text-muted">Search or jump…</span>
      <kbd class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-strong text-iso-text-secondary bg-iso-bg-base">⌘K</kbd>
    </button>

    <button class="w-8 h-7 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle flex items-center justify-center hover:border-iso-border-strong" title="Settings">
      <Icon name="lucide:settings" class="w-4 h-4 text-iso-text-muted" />
    </button>
  </div>
</template>

<script setup lang="ts">
const ui = useUiStore()

const tabs = [
  { path: '/', label: 'Home' },
  { path: '/hosts', label: 'Hosts' },
  { path: '/stacks', label: 'Stacks' },
  { path: '/events', label: 'Events' },
]
</script>
```

- [ ] **Step 2: FleetPicker.vue**

```vue
<template>
  <div class="relative">
    <button
      class="px-2.5 py-1 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm font-medium text-iso-text-secondary flex items-center gap-1.5 hover:border-iso-border-strong"
      @click="open = !open"
    >
      {{ activeLabel }}
      <Icon name="lucide:chevron-down" class="w-3 h-3 text-iso-text-faint" />
    </button>

    <div
      v-if="open"
      class="absolute top-full left-0 mt-1 min-w-48 bg-iso-bg-overlay border border-iso-border-strong rounded-iso-md shadow-lg z-50 py-1"
      @click.outside="open = false"
    >
      <button
        class="w-full text-left px-3 py-1.5 text-iso-sm hover:bg-iso-bg-row-hover"
        :class="ui.activeFleet === 'all' ? 'text-iso-text-primary' : 'text-iso-text-muted'"
        @click="select('all')"
      >
        All fleets
      </button>
      <div class="h-px bg-iso-border-subtle my-1"></div>
      <button
        v-for="f in fleetsStore.fleets"
        :key="f.name"
        class="w-full text-left px-3 py-1.5 text-iso-sm flex items-center justify-between hover:bg-iso-bg-row-hover"
        :class="ui.activeFleet === f.name ? 'text-iso-text-primary' : 'text-iso-text-muted'"
        @click="select(f.name)"
      >
        <span>{{ f.name }}</span>
        <span class="text-iso-xs text-iso-text-faint">{{ f.host_count }} hosts</span>
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

const ui = useUiStore()
const fleetsStore = useFleetsStore()
const open = ref(false)

onMounted(() => fleetsStore.load())

const activeLabel = computed(() => {
  if (ui.activeFleet === 'all') return 'All fleets'
  return ui.activeFleet
})

function select(name: string) {
  ui.setActiveFleet(name)
  open.value = false
}
</script>
```

- [ ] **Step 3: BottomStatusBar.vue**

```vue
<template>
  <div class="h-8 border-t border-iso-border-subtle bg-iso-bg-elevated px-4 flex items-center gap-4 text-iso-xs">
    <div class="flex items-center gap-1.5">
      <div class="w-1.5 h-1.5 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-error'"></div>
      <span :class="connected ? 'text-iso-success' : 'text-iso-error'">
        {{ connected ? `live · ${eventCount} events today` : 'disconnected' }}
      </span>
    </div>
    <span class="text-iso-text-faint">controller v0.1.0-alpha</span>
    <div class="flex-1"></div>
    <span class="text-iso-text-faint font-mono">↑↓ navigate · ⌘K command · ? help</span>
  </div>
</template>

<script setup lang="ts">
const props = defineProps<{
  connected: boolean
  eventCount: number
}>()
</script>
```

- [ ] **Step 4: Build + commit**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): TopBar + FleetPicker + BottomStatusBar components"
```

---

## Task 3: StateStrip + Inspector

**Files:**
- Create: `web/components/StateStrip.vue`
- Create: `web/components/Inspector.vue`

- [ ] **Step 1: StateStrip.vue**

```vue
<template>
  <div class="m-3 p-4 rounded-iso-lg bg-iso-bg-elevated border border-iso-border-subtle">
    <!-- Header row -->
    <div class="flex items-center gap-3.5 mb-3.5">
      <!-- Status icon -->
      <div
        class="w-6 h-6 rounded-full flex items-center justify-center"
        :style="{ backgroundColor: iconBg }"
      >
        <Icon :name="iconName" class="w-3.5 h-3.5" :style="{ color: iconColor }" />
      </div>

      <!-- Text col -->
      <div>
        <h4 class="font-semibold text-iso-md text-iso-text-primary">Fleet · {{ fleet.name }}</h4>
        <div class="flex gap-3 mt-1 text-iso-sm">
          <span v-if="healthyCount > 0" class="text-iso-success">{{ healthyCount }} healthy</span>
          <span v-if="updatingCount > 0" class="text-iso-warn">{{ updatingCount }} updating</span>
          <span v-if="failedCount > 0" class="text-iso-error">{{ failedCount }} failed</span>
        </div>
        <div class="text-iso-xs text-iso-text-muted mt-1">
          {{ totalServices }} services · {{ fleet.host_count }} hosts · last cycle {{ lastCycleAgo }}
        </div>
      </div>

      <div class="flex-1"></div>

      <!-- Mini-grid -->
      <div class="flex gap-1">
        <div v-for="(s, i) in serviceStates" :key="i" class="w-3.5 h-3.5 rounded-sm" :style="{ backgroundColor: stateColor(s) }"></div>
      </div>
    </div>

    <!-- Issues section -->
    <div v-if="issues.length > 0">
      <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">{{ issues.length }} need attention</div>
      <div class="space-y-1">
        <div v-for="issue in issues" :key="issue.id" class="flex items-center gap-2.5 py-1.5 border-b border-iso-border-subtle last:border-b-0 text-iso-sm">
          <div class="w-2 h-2 rounded-full" :style="{ backgroundColor: stateColor(issue.state), boxShadow: `0 0 6px ${stateColor(issue.state)}66` }"></div>
          <span class="text-iso-text-primary">
            {{ issue.container_name }}
            <span class="text-iso-text-faint font-mono text-iso-xs">on {{ issue.host_name }}</span>
          </span>
          <div class="flex-1"></div>
          <span class="text-iso-text-muted font-mono text-iso-xs">{{ issue.detail }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { Fleet } from '~/stores/fleets'

const props = defineProps<{
  fleet: Fleet
}>()

const eventsStore = useEventsStore()

// Derive state per service. v1 placeholder: from recent events for this fleet.
// In real impl: agent reports a stack/service snapshot; we read from /api/v1/services.
const serviceStates = computed<Array<'success'|'warn'|'error'|'info'>>(() => {
  // TODO 5d: replace with real service snapshot from API.
  // Placeholder: return mostly success with event-derived issues.
  const states: Array<'success'|'warn'|'error'|'info'> = []
  for (let i = 0; i < totalServices.value; i++) states.push('success')
  // Apply known-bad states from issues
  let idx = 0
  for (const issue of issues.value) {
    states[idx % states.length] = issue.state
    idx++
  }
  return states
})

const totalServices = computed(() => 7) // 5d: real count from API

const issues = computed(() => {
  const recent = eventsStore.events.slice(0, 50)
  return recent
    .filter(e => ['update.failed', 'update.pulling', 'agent.disconnect_long'].includes(e.kind))
    .filter(e => /* TODO: filter by fleet */ true)
    .slice(0, 5)
    .map(e => ({
      id: e.id,
      container_name: e.container_name ?? '?',
      host_name: '?', // TODO 5d: lookup from hosts store
      detail: e.summary,
      state: kindToState(e.kind),
    }))
})

function kindToState(kind: string): 'success'|'warn'|'error'|'info' {
  if (kind === 'update.failed') return 'error'
  if (kind === 'update.pulling') return 'warn'
  if (kind === 'agent.disconnect_long') return 'info'
  return 'success'
}

const healthyCount = computed(() => Math.max(0, totalServices.value - updatingCount.value - failedCount.value))
const updatingCount = computed(() => issues.value.filter(i => i.state === 'warn').length)
const failedCount = computed(() => issues.value.filter(i => i.state === 'error').length)

const iconName = computed(() => {
  if (failedCount.value > 0 || updatingCount.value > 0) return 'lucide:triangle-alert'
  return 'lucide:check'
})

const iconBg = computed(() => {
  if (failedCount.value > 0) return '#f8717126'
  if (updatingCount.value > 0) return '#fbbf2426'
  return '#4ade8026'
})

const iconColor = computed(() => {
  if (failedCount.value > 0) return '#f87171'
  if (updatingCount.value > 0) return '#fbbf24'
  return '#4ade80'
})

const lastCycleAgo = computed(() => '22s ago') // TODO 5d: derive from latest CHECKED event

function stateColor(s: string) {
  const map: Record<string, string> = {
    success: '#4ade80',
    warn: '#fbbf24',
    error: '#f87171',
    info: '#c084fc',
  }
  return map[s] ?? '#94a3b8'
}
</script>
```

- [ ] **Step 2: Inspector.vue**

```vue
<template>
  <div class="border-l border-iso-border-subtle bg-iso-bg-elevated p-4 overflow-y-auto" v-if="event">
    <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint">SELECTED EVENT · {{ formatTime(event.occurred_at) }}</div>

    <div class="flex items-center gap-2 mt-2 mb-1">
      <div class="w-2 h-2 rounded-full" :style="{ backgroundColor: kindColor }"></div>
      <h4 class="text-lg font-semibold text-iso-text-primary">{{ event.container_name ?? event.kind }}</h4>
    </div>

    <div class="text-iso-xs text-iso-text-muted font-mono mb-4">{{ event.image ?? event.summary }}</div>

    <!-- KV block -->
    <div class="space-y-1.5 mb-4">
      <KvRow label="Status" :value="event.kind" :value-class="kindTextClass" />
      <KvRow v-if="event.container_name" label="Container" :value="`/${event.container_name}`" mono />
      <KvRow v-if="event.image" label="Image" :value="event.image" mono />
    </div>

    <!-- Digest change block -->
    <div v-if="event.old_digest || event.new_digest" class="mb-4">
      <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">DIGEST CHANGE</div>
      <div v-if="event.old_digest" class="text-iso-xs font-mono p-2 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-text-faint">
        was&nbsp;&nbsp;{{ truncDigest(event.old_digest) }}
      </div>
      <div class="text-center text-iso-xs text-iso-text-faint my-1">↓</div>
      <div v-if="event.new_digest" class="text-iso-xs font-mono p-2 rounded-iso-sm bg-iso-bg-overlay border text-iso-success" style="border-color: #1e3826">
        now {{ truncDigest(event.new_digest) }}
      </div>
    </div>

    <hr class="border-iso-border-subtle my-4" />

    <!-- Quick actions -->
    <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">QUICK ACTIONS</div>
    <div class="space-y-1.5">
      <button class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Open container detail →
      </button>
      <button v-if="event.host_id" class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Open host detail →
      </button>
      <button class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Filter timeline to this container
      </button>
    </div>
  </div>
  <div v-else class="border-l border-iso-border-subtle bg-iso-bg-elevated p-6 text-iso-text-faint text-iso-sm">
    Select an event to see details.
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const ui = useUiStore()
const eventsStore = useEventsStore()

const event = computed(() => {
  if (ui.selectedEventId === null) return null
  return eventsStore.events.find(e => e.id === ui.selectedEventId) ?? null
})

const kindColor = computed(() => {
  if (!event.value) return '#94a3b8'
  return kindToColor(event.value.kind)
})

const kindTextClass = computed(() => {
  if (!event.value) return ''
  const k = event.value.kind
  if (k === 'update.success') return 'text-iso-success'
  if (k === 'update.failed') return 'text-iso-error'
  if (k === 'update.pulling') return 'text-iso-warn'
  if (k === 'agent.disconnect_long') return 'text-iso-info'
  return 'text-iso-neutral'
})

function kindToColor(k: string) {
  if (k === 'update.success') return '#4ade80'
  if (k === 'update.failed') return '#f87171'
  if (k === 'update.pulling') return '#fbbf24'
  if (k === 'agent.disconnect_long') return '#c084fc'
  return '#94a3b8'
}

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString()
}

function truncDigest(d: string) {
  if (d.length <= 24) return d
  return d.slice(0, 16) + '…'
}
</script>
```

- [ ] **Step 3: KvRow inline component (in Inspector.vue or standalone)**

For brevity, inline a tiny `<KvRow>` at the bottom of `Inspector.vue` script:

```vue
<!-- Add to template, replacing references -->
<!-- Use a Vue helper component definition or inline -->
```

Actually create a separate `web/components/KvRow.vue`:

```vue
<template>
  <div class="grid grid-cols-[90px_1fr] gap-3 text-iso-sm">
    <span class="text-iso-text-faint">{{ label }}</span>
    <span :class="[mono ? 'font-mono text-iso-xs' : '', valueClass ?? 'text-iso-text-secondary']">
      {{ value }}
    </span>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  label: string
  value: string
  mono?: boolean
  valueClass?: string
}>()
</script>
```

- [ ] **Step 4: Build + commit**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): StateStrip (calm/loud) + Inspector (event detail) + KvRow"
```

---

## Task 4: EventTimeline + EventRow + DayLabel

**Files:**
- Create: `web/components/EventTimeline.vue`
- Create: `web/components/EventRow.vue`
- Create: `web/components/DayLabel.vue`

- [ ] **Step 1: DayLabel.vue**

```vue
<template>
  <div class="px-5 pt-3.5 pb-1.5 text-iso-xs uppercase tracking-wider font-medium text-iso-text-faint">
    {{ label }}
  </div>
</template>

<script setup lang="ts">
defineProps<{ label: string }>()
</script>
```

- [ ] **Step 2: EventRow.vue**

```vue
<template>
  <button
    class="w-full grid grid-cols-[60px_90px_1fr_auto] gap-3.5 px-5 py-2 items-center text-left transition-colors border-l-2"
    :class="selected ? 'bg-iso-bg-selected border-iso-success' : 'border-transparent hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
  >
    <span class="font-mono text-iso-xs text-iso-text-faint">{{ formatTime(event.occurred_at) }}</span>
    <span class="font-mono text-iso-xs font-medium" :class="kindClass">{{ kindLabel }}</span>
    <span class="text-iso-base text-iso-text-secondary truncate">
      <span v-if="event.container_name" class="font-medium text-iso-text-primary">{{ event.container_name }}</span>
      <span v-if="event.summary" class="ml-1">{{ event.summary }}</span>
    </span>
    <span class="text-iso-xs text-iso-text-faint font-mono">{{ shortHostId }}</span>
  </button>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { EventRow as EventType } from '~/stores/events'

const props = defineProps<{
  event: EventType
  selected: boolean
}>()
defineEmits<{ select: [] }>()

const kindLabel = computed(() => props.event.kind.split('.')[1]?.toUpperCase() ?? props.event.kind.toUpperCase())

const kindClass = computed(() => {
  const k = props.event.kind
  if (k.startsWith('update.success')) return 'text-iso-success'
  if (k.startsWith('update.failed')) return 'text-iso-error'
  if (k.startsWith('update.pulling')) return 'text-iso-warn'
  if (k.startsWith('update.checked')) return 'text-iso-neutral'
  if (k.startsWith('agent.disconnect')) return 'text-iso-info'
  return 'text-iso-neutral'
})

const shortHostId = computed(() => {
  if (!props.event.host_id) return ''
  return props.event.host_id.slice(0, 6)
})

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString([], { hour12: false })
}
</script>
```

- [ ] **Step 3: EventTimeline.vue**

```vue
<template>
  <div class="overflow-y-auto h-full">
    <template v-for="(group, label) in grouped" :key="label">
      <DayLabel :label="label" />
      <EventRow
        v-for="e in group"
        :key="e.id"
        :event="e"
        :selected="ui.selectedEventId === e.id"
        @select="ui.selectEvent(e.id)"
      />
    </template>
    <div v-if="eventsStore.events.length === 0" class="p-8 text-center text-iso-text-faint text-iso-sm">
      No events yet.
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const eventsStore = useEventsStore()
const ui = useUiStore()

const grouped = computed(() => {
  const groups: Record<string, typeof eventsStore.events> = {}
  for (const e of eventsStore.events) {
    const date = new Date(e.occurred_at)
    const today = new Date()
    let label: string
    if (sameDay(date, today)) label = 'TODAY · ' + dateLabel(date)
    else if (sameDay(date, dayBefore(today))) label = 'YESTERDAY · ' + dateLabel(date)
    else label = dateLabel(date).toUpperCase()
    if (!groups[label]) groups[label] = []
    groups[label].push(e)
  }
  return groups
})

function sameDay(a: Date, b: Date) {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
}

function dayBefore(d: Date) {
  const x = new Date(d)
  x.setDate(x.getDate() - 1)
  return x
}

function dateLabel(d: Date) {
  return d.toLocaleDateString(undefined, { month: 'long', day: 'numeric' })
}
</script>
```

- [ ] **Step 4: Build + commit**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): EventTimeline + EventRow + DayLabel components"
```

---

## Task 5: CmdPane (navigator mode only)

**Files:**
- Create: `web/components/CmdPane.vue`
- Create: `web/components/CmdInput.vue`
- Create: `web/components/CmdResultRow.vue`
- Create: `web/components/CmdSection.vue`

- [ ] **Step 1: CmdSection.vue (small label)**

```vue
<template>
  <div class="px-5 pt-2.5 pb-1 text-iso-xs uppercase tracking-wider font-medium text-iso-text-faint">
    {{ label }}
  </div>
</template>

<script setup lang="ts">
defineProps<{ label: string }>()
</script>
```

- [ ] **Step 2: CmdInput.vue**

```vue
<template>
  <div class="flex items-center gap-3.5 h-15 px-5 border-b border-iso-border-subtle">
    <Icon name="lucide:search" class="w-4.5 h-4.5 text-iso-text-muted" />
    <input
      ref="inputRef"
      v-model="query"
      type="text"
      placeholder="Type to navigate, run, or shell…"
      class="flex-1 bg-transparent text-iso-text-primary text-lg outline-none placeholder:text-iso-text-faint"
      @keydown="$emit('keydown', $event)"
    />
    <kbd class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">esc</kbd>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'

const props = defineProps<{ modelValue: string }>()
const emit = defineEmits<{ 'update:modelValue': [v: string], keydown: [e: KeyboardEvent] }>()

const inputRef = ref<HTMLInputElement>()
const query = ref(props.modelValue)

watch(query, v => emit('update:modelValue', v))
watch(() => props.modelValue, v => { query.value = v })

onMounted(() => inputRef.value?.focus())
</script>
```

- [ ] **Step 3: CmdResultRow.vue**

```vue
<template>
  <button
    class="w-full flex items-center gap-3 px-5 py-2 transition-colors text-left"
    :class="highlighted ? 'bg-iso-bg-row-hover' : 'hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
  >
    <Icon :name="icon" class="w-3.5 h-3.5 text-iso-text-muted" />
    <span class="text-iso-base" :class="highlighted ? 'text-iso-text-primary font-medium' : 'text-iso-text-secondary'">{{ label }}</span>
    <span v-if="meta" class="text-iso-xs font-mono text-iso-text-faint">{{ meta }}</span>
    <div class="flex-1"></div>
    <kbd v-if="highlighted" class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">⏎ open</kbd>
  </button>
</template>

<script setup lang="ts">
defineProps<{
  icon: string
  label: string
  meta?: string
  highlighted?: boolean
}>()
defineEmits<{ select: [] }>()
</script>
```

- [ ] **Step 4: CmdPane.vue (navigator only)**

```vue
<template>
  <Teleport to="body">
    <div v-if="ui.cmdPaneOpen" class="fixed inset-0 z-50 flex items-start justify-center bg-black/60 backdrop-blur-sm pt-[180px]" @click.self="ui.closeCmdPane()">
      <div class="w-[640px] max-w-full bg-iso-bg-overlay border border-iso-border-strong rounded-iso-lg shadow-2xl overflow-hidden flex flex-col" @click.stop style="max-height: 70vh">
        <CmdInput v-model="query" @keydown="onKey" />

        <div class="flex-1 overflow-y-auto py-1.5">
          <template v-if="results.hosts.length > 0">
            <CmdSection label="Hosts" />
            <CmdResultRow
              v-for="(h, i) in results.hosts"
              :key="`h-${h.id}`"
              icon="lucide:server"
              :label="h.hostname"
              :meta="`${h.fleet} · ${h.fingerprint.slice(0, 12)}`"
              :highlighted="selectedIdx === i"
              @select="navigateToHost(h)"
            />
          </template>

          <template v-if="results.events.length > 0">
            <CmdSection label="Events" />
            <CmdResultRow
              v-for="(e, i) in results.events"
              :key="`e-${e.id}`"
              icon="lucide:activity"
              :label="e.summary"
              :meta="e.kind"
              :highlighted="selectedIdx === results.hosts.length + i"
              @select="selectEvent(e)"
            />
          </template>

          <template v-if="results.actions.length > 0">
            <CmdSection label="Actions" />
            <CmdResultRow
              v-for="(a, i) in results.actions"
              :key="`a-${a.label}`"
              :icon="a.icon"
              :label="a.label"
              :meta="a.meta"
              :highlighted="selectedIdx === results.hosts.length + results.events.length + i"
              @select="a.run()"
            />
          </template>

          <div v-if="totalResults === 0 && query.length > 0" class="px-5 py-6 text-center text-iso-text-faint text-iso-sm">
            No matches. Try a host, event, or action.
          </div>
          <div v-if="totalResults === 0 && query.length === 0" class="px-5 py-3 text-iso-sm text-iso-text-muted">
            <p>Type to search hosts, events, or run actions.</p>
            <p class="text-iso-xs mt-2 text-iso-text-faint">: for actions · $ for shell · ? for help</p>
          </div>
        </div>

        <div class="h-9 px-4 border-t border-iso-border-subtle flex items-center gap-3.5 text-iso-xs font-mono text-iso-text-faint">
          <span>↑↓ navigate</span>
          <span>⏎ select</span>
          <div class="flex-1"></div>
          <span>⌘. dock · esc close</span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import Fuse from 'fuse.js'

const ui = useUiStore()
const router = useRouter()
const hostsStore = useHostsStore()
const eventsStore = useEventsStore()

const query = ref('')
const selectedIdx = ref(0)

watch(() => ui.cmdPaneOpen, (open) => {
  if (open) {
    query.value = ''
    selectedIdx.value = 0
  }
})

const hostFuse = computed(() => new Fuse(hostsStore.hosts, { keys: ['hostname', 'fingerprint', 'fleet'] }))
const eventFuse = computed(() => new Fuse(eventsStore.events, { keys: ['summary', 'kind', 'container_name'] }))

const results = computed(() => {
  if (query.value.length === 0) {
    return { hosts: hostsStore.hosts.slice(0, 5), events: [], actions: defaultActions.value }
  }
  return {
    hosts: hostFuse.value.search(query.value).slice(0, 5).map(r => r.item),
    events: eventFuse.value.search(query.value).slice(0, 5).map(r => r.item),
    actions: defaultActions.value.filter(a =>
      a.label.toLowerCase().includes(query.value.toLowerCase())
    ),
  }
})

const totalResults = computed(() => results.value.hosts.length + results.value.events.length + results.value.actions.length)

const defaultActions = computed(() => [
  { icon: 'lucide:zap', label: 'Force update cycle on all hosts', meta: 'runs now', run: () => alert('TODO 5d: wire force-update RPC') },
  { icon: 'lucide:terminal', label: 'Open shell on a container', meta: 'pick container next', run: () => alert('TODO 5e: cmd pane terminal mode') },
])

function onKey(e: KeyboardEvent) {
  if (e.key === 'ArrowDown') { e.preventDefault(); selectedIdx.value = Math.min(selectedIdx.value + 1, totalResults.value - 1) }
  else if (e.key === 'ArrowUp') { e.preventDefault(); selectedIdx.value = Math.max(0, selectedIdx.value - 1) }
  else if (e.key === 'Enter') { e.preventDefault(); selectActive() }
}

function selectActive() {
  const hostsLen = results.value.hosts.length
  const eventsLen = results.value.events.length
  const i = selectedIdx.value
  if (i < hostsLen) navigateToHost(results.value.hosts[i])
  else if (i < hostsLen + eventsLen) selectEvent(results.value.events[i - hostsLen])
  else {
    const actionIdx = i - hostsLen - eventsLen
    results.value.actions[actionIdx]?.run()
  }
}

function navigateToHost(h: any) {
  ui.closeCmdPane()
  router.push(`/hosts/${h.id}`)
}

function selectEvent(e: any) {
  ui.selectEvent(e.id)
  ui.closeCmdPane()
}
</script>
```

- [ ] **Step 5: Build + commit**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): CmdPane (navigator) + CmdInput + CmdResultRow + CmdSection"
```

---

## Task 6: Assemble pages/index.vue + WS prepend wiring

**Files:**
- Modify: `web/pages/index.vue`
- Modify: `web/stores/events.ts` (already has prepend; ensure useEventStream calls it)

- [ ] **Step 1: Wire useEventStream to prepend into store**

In `web/composables/useEventStream.ts`, replace the message handler block:

```typescript
socket.addEventListener('message', (msg) => {
  try {
    const frame = JSON.parse(msg.data)
    if (frame.type === 'event') {
      events.value.unshift(frame.event)
      if (events.value.length > 500) events.value.length = 500
      // Also push into the global Pinia store
      const eventsStore = useEventsStore()
      eventsStore.prepend(frame.event)
    }
  } catch { /* ignore */ }
})
```

- [ ] **Step 2: pages/index.vue full assembly**

```vue
<template>
  <div class="h-screen flex flex-col">
    <TopBar />
    <main class="flex-1 grid grid-cols-[1fr_340px] overflow-hidden">
      <div class="overflow-y-auto">
        <StateStrip
          v-for="f in fleetsToShow"
          :key="f.name"
          :fleet="f"
        />
        <EventTimeline />
      </div>
      <Inspector />
    </main>
    <BottomStatusBar :connected="connected" :event-count="eventsStore.events.length" />
    <CmdPane />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const fleetsStore = useFleetsStore()
const ui = useUiStore()
const { connected } = useEventStream()

onMounted(async () => {
  await Promise.all([
    eventsStore.load(100),
    hostsStore.load(),
    fleetsStore.load(),
  ])
})

const fleetsToShow = computed(() => {
  if (ui.activeFleet === 'all') return fleetsStore.fleets
  return fleetsStore.fleets.filter(f => f.name === ui.activeFleet)
})
</script>
```

- [ ] **Step 3: Build the bundle**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web && bun run build 2>&1 | tail -5
```

Expected: success.

- [ ] **Step 4: Build the workspace**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard/web): assemble Home view (TopBar + StateStrip + Timeline + Inspector + CmdPane)"
```

---

## Task 7: Manual smoke + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails, fix and re-commit.

- [ ] **Step 2: Manual smoke (controller + agent + browser)**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-5c-{ctrl,agent}
ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9417 --state-dir /tmp/isengard-5c-ctrl &
sleep 1
ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9417 --state-dir /tmp/isengard-5c-agent &
sleep 5
```

Open http://localhost:9418/ in browser. Expect to see:
- Top bar with isengard mark, "All fleets" picker, Home tab active, Hosts/Stacks/Events tabs (clickable but no destination yet), Atlas / ⌘K / Settings buttons on right
- One state strip card (Fleet · default) with the host count
- Event timeline empty initially (no events yet); when agent emits update.checked, it appears live
- Inspector says "Select an event to see details"
- Click an event → inspector populates
- Press ⌘K → cmd pane opens centered with backdrop
- Type host name → result appears under Hosts section
- Esc → cmd pane closes
- Bottom status bar shows live indicator + version + keyboard hints

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase5c -m "phase 5c: home view (state strip + timeline + inspector + cmd pane navigator)"
```

- [ ] **Step 4: Confirm done**

- [ ] All listed components render
- [ ] Live event flow works (event appears in timeline within ~1s of agent emit)
- [ ] Cmd pane keyboard shortcuts work
- [ ] Tag `v0.1.0-alpha.phase5c` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§4-§6, §9) | Plan task |
|---|---|
| Top bar (brand + fleet picker + tabs + atlas + ⌘K + settings) | Task 2 |
| State strip (calm/loud, exception-driven) | Task 3 |
| Event timeline (day-grouped, color-coded) | Task 4 |
| Inspector (event detail) | Task 3 |
| Cmd pane navigator (center, fuzzy search, kbd nav) | Task 5 |
| Home view assembly | Task 6 |
| Live event flow (WS → store → UI) | Task 6 |
| Bottom status bar | Task 2 |

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-05-01-phase-5c-home-view.md`. Subagent-driven execution.
