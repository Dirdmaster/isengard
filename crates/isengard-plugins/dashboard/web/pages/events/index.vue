<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { useRouter } from 'vue-router'
import { useEventsStore, type EventRow as EventRowType } from '~/stores/events'
import { useHostsStore } from '~/stores/hosts'

const eventsStore = useEventsStore()
const hostsStore  = useHostsStore()

await Promise.all([
  eventsStore.load(500),
  hostsStore.load(),
])

const router = useRouter()

// ─── Live tail ────────────────────────────────────────────────────────────
//
// Default ON. When OFF we snapshot the rendered list and surface a "X new
// events" banner; toggling back on flushes the freeze. The store itself is
// shared (StateStrip / dashboard / etc. all read it) so we can't pause the
// underlying prepend — only what this page renders.
const liveTail = ref(true)
const frozenEvents = ref<EventRowType[]>([])
const frozenLen = ref(0)

function pauseTail() {
  frozenEvents.value = eventsStore.events.slice()
  frozenLen.value = eventsStore.events.length
  liveTail.value = false
}

function resumeTail() {
  frozenEvents.value = []
  frozenLen.value = 0
  liveTail.value = true
}

function toggleTail() {
  if (liveTail.value) pauseTail()
  else resumeTail()
}

const queuedCount = computed(() => {
  if (liveTail.value) return 0
  return Math.max(0, eventsStore.events.length - frozenLen.value)
})

watch(liveTail, (on) => {
  if (on) frozenEvents.value = []
})

const liveSourceEvents = computed<EventRowType[]>(() => {
  return liveTail.value ? eventsStore.events : frozenEvents.value
})

// ─── Time range (client-side) ─────────────────────────────────────────────
//
// Backend `list_events` only filters by kind/host/limit, so the picker is
// applied here over the already-fetched 500-event window. "all" means
// no filter beyond what was loaded.
type Range = '1h' | '24h' | '7d' | 'all'
const RANGES: { key: Range; label: string; ms: number | null }[] = [
  { key: '1h',  label: 'last 1h',  ms: 60 * 60 * 1000 },
  { key: '24h', label: 'last 24h', ms: 24 * 60 * 60 * 1000 },
  { key: '7d',  label: 'last 7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { key: 'all', label: 'all',      ms: null },
]
const range = ref<Range>('1h')

const rangeBounded = computed<EventRowType[]>(() => {
  const cfg = RANGES.find((r) => r.key === range.value)
  if (!cfg || cfg.ms === null) return liveSourceEvents.value
  const cutoff = Date.now() - cfg.ms
  return liveSourceEvents.value.filter((e) => {
    const t = new Date(e.occurred_at).getTime()
    return Number.isFinite(t) && t >= cutoff
  })
})

// ─── Kind filter (derived from visible events) ────────────────────────────
//
// Concept covers update.* / deploy.* / approval.* / agent.* / routing.* and
// more — the set is open-ended. We derive the kind families from the actual
// data within the current time range so the chip set always matches what's
// on screen.
function kindFamily(kind: string): string {
  const dot = kind.indexOf('.')
  return dot < 0 ? kind : kind.slice(0, dot)
}

function toneForFamily(family: string): 'success' | 'warn' | 'error' | 'info' | 'neutral' {
  switch (family) {
    case 'update':   return 'success'
    case 'deploy':   return 'info'
    case 'approval': return 'warn'
    case 'routing':  return 'success'
    case 'agent':    return 'info'
    case 'webhook':  return 'success'
    case 'backup':   return 'success'
    case 'policy':   return 'success'
    case 'hooks':    return 'info'
    case 'stack':    return 'info'
    default:         return 'neutral'
  }
}

const kindFamilies = computed(() => {
  const counts: Record<string, number> = {}
  for (const e of rangeBounded.value) {
    const fam = kindFamily(e.kind)
    counts[fam] = (counts[fam] ?? 0) + 1
  }
  return Object.entries(counts)
    .map(([family, count]) => ({ family, count, tone: toneForFamily(family) }))
    .sort((a, b) => b.count - a.count)
})

// `null` means "all kinds visible". A non-null Set means "only these families".
const activeFamilies = ref<Set<string> | null>(null)

function toggleFamily(family: string) {
  // If everything is shown and the user clicks one chip, isolate to that one.
  if (activeFamilies.value === null) {
    activeFamilies.value = new Set([family])
    return
  }
  const set = new Set(activeFamilies.value)
  if (set.has(family)) set.delete(family)
  else set.add(family)
  // Empty set = back to "all" so the user can't filter into a black hole.
  if (set.size === 0) activeFamilies.value = null
  else activeFamilies.value = set
}

function isFamilyActive(family: string): boolean {
  if (activeFamilies.value === null) return true
  return activeFamilies.value.has(family)
}

// ─── Host filter ──────────────────────────────────────────────────────────
const activeHostId = ref<string | null>(null)

// ─── Free-text search ─────────────────────────────────────────────────────
const search = ref('')

function matchesSearch(e: EventRowType, q: string): boolean {
  if (!q) return true
  const needle = q.toLowerCase()
  if (e.kind?.toLowerCase().includes(needle)) return true
  if (e.host_id?.toLowerCase().includes(needle)) return true
  if (e.container_name?.toLowerCase().includes(needle)) return true
  if (e.summary?.toLowerCase().includes(needle)) return true
  if (e.metadata) {
    for (const v of Object.values(e.metadata)) {
      if (typeof v === 'string' && v.toLowerCase().includes(needle)) return true
    }
  }
  return false
}

// ─── Final filter chain ──────────────────────────────────────────────────
const filtered = computed(() => {
  const q = search.value.trim()
  return rangeBounded.value.filter((e) => {
    if (!isFamilyActive(kindFamily(e.kind))) return false
    if (activeHostId.value && e.host_id !== activeHostId.value) return false
    if (q && !matchesSearch(e, q)) return false
    return true
  })
})

// ─── Subtitle: rate readout matching concept ("82 events in the last hour
// · 1.4 / sec average"). Computed off the *current* range so it reads true.
const subtitle = computed(() => {
  const cfg = RANGES.find((r) => r.key === range.value)
  const n = rangeBounded.value.length
  if (n === 0) return `No events in ${cfg?.label ?? 'range'} · ${hostsStore.hosts.length} hosts heartbeating`
  if (!cfg || cfg.ms === null) {
    return `${n} events loaded`
  }
  const seconds = cfg.ms / 1000
  const rate = n / seconds
  const rateStr = rate >= 1 ? `${rate.toFixed(1)} / sec` : `${(rate * 60).toFixed(1)} / min`
  return `${n} events in ${cfg.label} · ${rateStr} average`
})

// ─── Row click: jump to relevant entity per page-spec ─────────────────────
//
// Per `design/pages/events.md`, clicking an event should jump to the related
// entity (host / service / approval) — not a generic /events/[id] detail.
// We honour that here: prefer the host route when there's a host_id, else
// fall back to the existing detail page.
function openEvent(e: EventRowType) {
  if (e.host_id) {
    router.push(`/hosts/${e.host_id}`)
    return
  }
  router.push(`/events/${e.id}`)
}
</script>

<template>
  <AppShell>
    <main class="flex-1 flex flex-col min-h-0 px-6 pt-5 pb-4 gap-4 overflow-hidden">
      <!-- Page header: title + live badge + rate readout, actions on the right. -->
      <div class="flex items-center justify-between shrink-0">
        <div class="flex flex-col gap-1">
          <div class="flex items-center gap-2">
            <h1 class="text-[22px] font-semibold text-iso-text-primary">Events</h1>
            <span
              class="px-2 py-0.5 rounded-iso-sm border font-mono text-[11px]"
              :class="liveTail
                ? 'bg-iso-success-soft border-iso-success/40 text-iso-success'
                : 'bg-iso-bg-overlay border-iso-border-subtle text-iso-text-muted'"
            >
              <span
                class="inline-block w-1.5 h-1.5 rounded-full mr-1 align-middle"
                :class="liveTail ? 'bg-iso-success animate-pulse' : 'bg-iso-text-muted'"
              ></span>
              {{ liveTail ? 'live' : 'paused' }}
            </span>
          </div>
          <span class="text-xs text-iso-text-muted">{{ subtitle }}</span>
        </div>
        <div class="flex items-center gap-2">
          <button
            type="button"
            class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:text-iso-text-primary transition-colors"
            @click="toggleTail"
          >
            {{ liveTail ? 'Pause' : 'Resume' }}
          </button>
          <button
            type="button"
            disabled
            title="Backend export endpoint coming soon"
            class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle text-xs text-iso-text-faint cursor-not-allowed"
          >
            Export JSONL
          </button>
        </div>
      </div>

      <!-- Filter bar: kind chips + range segmented control + search. -->
      <div class="flex items-center gap-3 px-3 py-2 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle shrink-0 flex-wrap">
        <span class="text-[10px] font-semibold text-iso-text-muted tracking-wider">KIND</span>
        <div class="flex items-center gap-1.5 flex-wrap">
          <EventFilterChip
            v-for="f in kindFamilies"
            :key="f.family"
            :label="`${f.family}.*`"
            :count="f.count"
            :active="isFamilyActive(f.family)"
            :tone="f.tone"
            @toggle="toggleFamily(f.family)"
          />
          <span v-if="kindFamilies.length === 0" class="font-mono text-[11px] text-iso-text-faint">no events</span>
        </div>

        <div class="w-px h-4 bg-iso-border-subtle"></div>

        <span class="text-[10px] font-semibold text-iso-text-muted tracking-wider">RANGE</span>
        <div class="inline-flex rounded-iso-sm border border-iso-border-subtle overflow-hidden">
          <button
            v-for="r in RANGES"
            :key="r.key"
            type="button"
            class="px-2 py-0.5 font-mono text-[11px] border-r last:border-r-0 border-iso-border-subtle transition-colors"
            :class="range === r.key
              ? 'bg-iso-bg-overlay text-iso-text-primary'
              : 'bg-iso-bg-base text-iso-text-secondary hover:text-iso-text-primary'"
            @click="range = r.key"
          >{{ r.label }}</button>
        </div>

        <div class="w-px h-4 bg-iso-border-subtle"></div>

        <select
          v-model="activeHostId"
          class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-sm px-2 py-0.5 font-mono text-[11px] text-iso-text-secondary"
        >
          <option :value="null">all hosts</option>
          <option v-for="h in hostsStore.hosts" :key="h.id" :value="h.id">{{ h.hostname }}</option>
        </select>

        <div class="flex-1"></div>

        <div class="flex items-center gap-2 px-2 py-1 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle min-w-[180px]">
          <Icon name="lucide:search" class="w-3 h-3 text-iso-text-muted" />
          <input
            v-model="search"
            type="text"
            placeholder="search payload…"
            class="bg-transparent text-xs text-iso-text-primary outline-none flex-1 placeholder:text-iso-text-faint"
          />
        </div>
      </div>

      <!-- Live-tail "X new events" banner when paused. -->
      <button
        v-if="!liveTail && queuedCount > 0"
        type="button"
        class="w-full flex items-center justify-center gap-2 px-4 py-2 bg-iso-info-soft hover:bg-iso-info/20 border border-iso-info/30 rounded-iso-md text-xs text-iso-info font-medium transition-colors shrink-0"
        @click="resumeTail"
      >
        <Icon name="lucide:arrow-up" class="w-3.5 h-3.5" />
        {{ queuedCount }} new {{ queuedCount === 1 ? 'event' : 'events' }} — click to resume
      </button>

      <!-- Event list: header row + scrollable body. -->
      <div
        v-if="filtered.length > 0"
        class="flex-1 rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden flex flex-col min-h-0"
      >
        <div class="grid grid-cols-[80px_180px_1fr_140px] gap-3 px-4 py-2.5 text-[10px] font-semibold tracking-wider text-iso-text-muted border-b border-iso-border-subtle shrink-0">
          <div>TIME</div>
          <div>KIND</div>
          <div>MESSAGE</div>
          <div>TARGET</div>
        </div>
        <div class="flex-1 overflow-auto min-h-0">
          <EventRow
            v-for="e in filtered"
            :key="e.id"
            :event="e"
            :selected="false"
            @select="openEvent(e)"
          />
        </div>
      </div>

      <!-- Empty: no events at all match (after every filter). Mirrors empty-v1.html. -->
      <EventsEmptyState
        v-else-if="liveSourceEvents.length === 0"
        :host-count="hostsStore.hosts.length"
      />
      <div
        v-else
        class="flex-1 rounded-iso-xl border border-dashed border-iso-border-subtle bg-iso-bg-elevated/40 flex flex-col items-center justify-center px-6 py-12 gap-2 min-h-0"
      >
        <p class="text-sm text-iso-text-muted">No events match the current filter.</p>
        <p class="text-xs text-iso-text-faint">Try widening the range, clearing kind chips, or removing the search.</p>
      </div>
    </main>
  </AppShell>
</template>
