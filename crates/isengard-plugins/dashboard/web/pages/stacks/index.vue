<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useEventsStore } from '~/stores/events'
import { useUiStore } from '~/stores/ui'

const stacksStore = useStacksStore()
const hostsStore  = useHostsStore()
const eventsStore = useEventsStore()
const uiStore     = useUiStore()

await Promise.all([
  stacksStore.fetchAll(),
  hostsStore.load(),
  eventsStore.load(200),
])

const rows = computed(() => {
  const fleet = uiStore.activeFleet
  return stacksStore.items
    .map((stack) => {
      const host = hostsStore.hosts.find((h) => h.id === stack.host_id)
      if (!host) return null
      if (fleet !== 'all' && host.fleet !== fleet) return null
      const latest = eventsStore.events.find((e) => e.host_id === stack.host_id) ?? null
      return {
        stack,
        hostHostname: host.hostname,
        fleet: host.fleet,
        serviceCount: 0, // 5e: real service count
        latestEvent: latest ? { kind: latest.kind, summary: latest.summary } : null,
      }
    })
    .filter((r): r is NonNullable<typeof r> => r !== null)
})
</script>

<template>
  <div class="flex flex-col h-full">
    <TopBar />
    <div class="px-4 py-3 border-b border-iso-border-subtle">
      <div class="text-sm text-iso-text-muted">{{ rows.length }} stacks</div>
    </div>
    <TableSkeleton v-if="!stacksStore.loaded" :rows="6" :columns="[200, 170, 70, 70, 400, 90]" />
    <StacksTable v-else :rows="rows" />
    <div class="mt-auto px-4 py-2 text-xs text-iso-text-faint border-t border-iso-border-subtle">
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">/</kbd> filter ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">⌘K</kbd> cmd
    </div>
  </div>
</template>
