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
  <AppShell>
    <PageHeader title="Events" :subtitle="`${filtered.length} of ${eventsStore.events.length}`">
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
        <select
          v-model="activeHostId"
          class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-1 text-xs"
        >
          <option :value="null">All hosts</option>
          <option v-for="h in hostsStore.hosts" :key="h.id" :value="h.id">{{ h.hostname }}</option>
        </select>
      </template>
    </PageHeader>

    <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
      <EmptyState
        v-if="filtered.length === 0 && eventsStore.events.length === 0"
        icon="activity"
        title="All quiet"
        description="Events appear as Isengard checks for image updates and applies them. Quiet is the default state. Nothing here means nothing has changed."
      />
      <div
        v-else-if="filtered.length === 0"
        class="flex-1 flex flex-col items-center justify-center px-6 py-12 gap-2"
      >
        <p class="text-sm text-iso-text-muted">No events match the current filter.</p>
        <p class="text-xs text-iso-text-faint">Try toggling more kinds or clearing the host filter.</p>
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
