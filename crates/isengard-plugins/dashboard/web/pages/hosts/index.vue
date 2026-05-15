<script setup lang="ts">
import { computed, ref } from 'vue'
import { useHostsStore, type Host } from '~/stores/hosts'
import { useStacksStore } from '~/stores/stacks'
import { useEventsStore } from '~/stores/events'
const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()

const inspectingHost = ref<Host | null>(null)

await Promise.all([
  hostsStore.load(),
  stacksStore.fetchAll(),
  eventsStore.load(200),
])

// kill-fleets: no fleet filter. The whole hosts list is rendered.
const filteredHosts = computed(() => hostsStore.hosts)

const subtitle = computed(() => {
  const n = filteredHosts.value.length
  const noun = n === 1 ? 'host' : 'hosts'
  return `${n} ${noun}`
})

const stackCounts = computed(() => {
  const out: Record<string, { stacks: number; services: number }> = {}
  for (const h of hostsStore.hosts) {
    const hostStacks = stacksStore.byHost(h.id)
    out[h.id] = { stacks: hostStacks.length, services: 0 }
  }
  return out
})

const latestEvents = computed(() => {
  const out: Record<string, { kind: string; summary: string } | null> = {}
  for (const h of hostsStore.hosts) {
    const e = eventsStore.events.find((ev) => ev.host_id === h.id) ?? null
    out[h.id] = e ? { kind: e.kind, summary: e.summary } : null
  }
  return out
})

// Per `design/concepts/hosts/inspector-v1.html` and `design/pages/hosts.md`,
// clicking a hostname opens the HostInspector slide-over, not a route push.
function selectHost(host: Host) {
  inspectingHost.value = host
}

async function handleAction(action: 'force-update' | 'shell' | 'menu', host: Host) {
  if (action === 'menu') {
    inspectingHost.value = host
  } else if (action === 'force-update') {
    try {
      await useHostActions().forceUpdate(host.id)
      useToast().success(`Force update queued for ${host.hostname}`)
    } catch (e) {
      useToast().error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
    }
  }
}
</script>

<template>
  <AppShell>
    <PageHeader title="Hosts" :subtitle="subtitle">
      <template #actions>
        <AddHostButton />
      </template>
    </PageHeader>
    <div class="flex-1 flex flex-col min-h-0 p-4 overflow-y-auto">
      <TableSkeleton v-if="!hostsStore.loaded" :rows="6" />
      <HostsTable
        v-else
        :hosts="filteredHosts"
        :stack-counts="stackCounts"
        :latest-events="latestEvents"
        :selected-id="inspectingHost?.id ?? null"
        class="flex-1 flex flex-col min-h-0"
        @select="selectHost"
        @action="handleAction"
      />
    </div>

    <HostInspector
      v-if="inspectingHost"
      :host="inspectingHost"
      @close="inspectingHost = null"
      @changed="hostsStore.load()"
    />
  </AppShell>
</template>
