<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { useEventsStore, type EventRow as EventRowType } from '~/stores/events'
import { useHostsStore } from '~/stores/hosts'

const eventsStore = useEventsStore()
const hostsStore  = useHostsStore()

await Promise.all([
  eventsStore.load(500),
  hostsStore.load(),
])

const KINDS = ['UPDATED', 'FAILED', 'CHECKED', 'PULLING', 'DISCONNECT'] as const
const activeKinds = ref(new Set<string>(KINDS))
const activeHostId = ref<string | null>(null)
const search = ref('')

// Live tail state. Default ON: new events stream into the view as they
// arrive on the WS. Toggle OFF to freeze the visible list and accumulate a
// "X new events" banner — click banner to flush + resume.
//
// The store is shared globally (StateStrip / Inspector both read it), so we
// can't pause the prepend itself; we snapshot the rendered list instead.
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

// If the user clears all filters or toggles off and on, ensure freeze drops.
watch(liveTail, (on) => {
  if (on) frozenEvents.value = []
})

const visibleEvents = computed<EventRowType[]>(() => {
  return liveTail.value ? eventsStore.events : frozenEvents.value
})

function toggleKind(kind: string) {
  if (activeKinds.value.has(kind)) activeKinds.value.delete(kind)
  else activeKinds.value.add(kind)
}

function kindKey(k: string): string {
  const upper = k.toUpperCase()
  if (upper.includes('UPDATE.SUCCESS')) return 'UPDATED'
  if (upper.includes('UPDATE.FAILED')) return 'FAILED'
  if (upper.includes('UPDATE.CHECKED')) return 'CHECKED'
  if (upper.includes('UPDATE.PULLING')) return 'PULLING'
  if (upper.includes('AGENT.DISCONNECT')) return 'DISCONNECT'
  return upper
}

const counts = computed(() => {
  const c: Record<string, number> = {}
  for (const k of KINDS) c[k] = 0
  for (const e of visibleEvents.value) {
    const k = kindKey(e.kind)
    if (k in c) c[k]++
  }
  return c
})

// Free-text search per `design/pages/events.md`. Matches against host_id,
// kind, container_name, summary, and any string value in metadata.
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

const filtered = computed(() => {
  const q = search.value.trim()
  return visibleEvents.value.filter((e) => {
    const k = kindKey(e.kind)
    if (!activeKinds.value.has(k)) return false
    if (activeHostId.value && e.host_id !== activeHostId.value) return false
    if (q && !matchesSearch(e, q)) return false
    return true
  })
})

const router = useRouter()
function openEvent(e: { id: number }) {
  router.push(`/events/${e.id}`)
}
</script>

<template>
  <AppShell>
    <PageHeader title="Events" :subtitle="`${filtered.length} of ${visibleEvents.length}`">
      <template #meta>
        <div class="flex items-center gap-2 flex-wrap">
          <EventFilterChip
            v-for="k in KINDS"
            :key="k"
            :label="k"
            :count="counts[k]"
            :active="activeKinds.has(k)"
            @toggle="toggleKind(k)"
          />
        </div>
      </template>
      <template #actions>
        <input
          v-model="search"
          type="text"
          placeholder="Search events…"
          class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-1 text-xs font-mono w-48 focus:outline-none focus:border-iso-success placeholder:text-iso-text-faint"
        />
        <select
          v-model="activeHostId"
          class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-1 text-xs"
        >
          <option :value="null">All hosts</option>
          <option v-for="h in hostsStore.hosts" :key="h.id" :value="h.id">{{ h.hostname }}</option>
        </select>
        <button
          type="button"
          class="inline-flex items-center gap-1.5 rounded border px-2 py-1 text-xs font-medium transition-colors"
          :class="liveTail
            ? 'border-iso-success/40 text-iso-success bg-iso-success/10 hover:bg-iso-success/15'
            : 'border-iso-border-subtle text-iso-text-muted hover:text-iso-text-primary'"
          :title="liveTail ? 'Pause incoming events' : 'Resume live tail'"
          @click="toggleTail"
        >
          <span
            class="w-1.5 h-1.5 rounded-full"
            :class="liveTail ? 'bg-iso-success animate-pulse' : 'bg-iso-text-muted'"
          ></span>
          {{ liveTail ? 'Live' : 'Paused' }}
        </button>
      </template>
    </PageHeader>

    <button
      v-if="!liveTail && queuedCount > 0"
      type="button"
      class="w-full flex items-center justify-center gap-2 px-4 py-2 bg-iso-info/10 hover:bg-iso-info/20 border-b border-iso-info/30 text-xs text-iso-info font-medium transition-colors shrink-0"
      @click="resumeTail"
    >
      <Icon name="lucide:arrow-up" class="w-3.5 h-3.5" />
      {{ queuedCount }} new {{ queuedCount === 1 ? 'event' : 'events' }} — click to resume
    </button>

    <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
      <EmptyState
        v-if="filtered.length === 0 && visibleEvents.length === 0"
        icon="activity"
        title="All quiet"
        description="Events appear as Isengard checks for image updates and applies them. Quiet is the default state. Nothing here means nothing has changed."
      />
      <div
        v-else-if="filtered.length === 0"
        class="flex-1 flex flex-col items-center justify-center px-6 py-12 gap-2"
      >
        <p class="text-sm text-iso-text-muted">No events match the current filter.</p>
        <p class="text-xs text-iso-text-faint">Try clearing the search, toggling more kinds, or clearing the host filter.</p>
      </div>
      <template v-else>
        <EventRow
          v-for="e in filtered"
          :key="e.id"
          :event="e"
          :selected="false"
          @select="openEvent(e)"
        />
      </template>
    </div>
  </AppShell>
</template>
