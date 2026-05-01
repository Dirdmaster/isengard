<script setup lang="ts">
import { ref, computed } from 'vue'
import { useEventsStore } from '~/stores/events'
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
  for (const e of eventsStore.events) {
    const k = kindKey(e.kind)
    if (k in c) c[k]++
  }
  return c
})

const filtered = computed(() => {
  return eventsStore.events.filter((e) => {
    const k = kindKey(e.kind)
    if (!activeKinds.value.has(k)) return false
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
  <div class="flex flex-col h-screen">
    <TopBar />
    <div class="flex items-center gap-2 px-4 py-3 border-b border-iso-border-subtle flex-wrap">
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
        class="ml-2 bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-1 text-xs"
      >
        <option :value="null">All hosts</option>
        <option v-for="h in hostsStore.hosts" :key="h.id" :value="h.id">{{ h.hostname }}</option>
      </select>
      <span class="ml-auto text-xs text-iso-text-muted">
        {{ filtered.length }} / {{ eventsStore.events.length }} events
      </span>
    </div>

    <div class="flex-1 overflow-y-auto">
      <EventRow
        v-for="e in filtered"
        :key="e.id"
        :event="e"
        :selected="false"
        @select="openEvent(e)"
      />
    </div>
  </div>
</template>
