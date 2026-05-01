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
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />
    <header class="flex items-center justify-between px-4 py-3 border-b border-iso-border-subtle">
      <div class="flex items-center gap-3">
        <h1 class="font-mono text-base text-iso-text-primary">Stacks</h1>
        <span class="text-xs text-iso-text-muted">
          {{ rows.length }} {{ rows.length === 1 ? 'stack' : 'stacks' }}
        </span>
      </div>
    </header>
    <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
      <TableSkeleton v-if="!stacksStore.loaded" :rows="6" :columns="[200, 170, 70, 70, 400, 90]" />
      <StacksTable v-else :rows="rows" class="flex-1 flex flex-col min-h-0" />
    </div>
  </div>
</template>
