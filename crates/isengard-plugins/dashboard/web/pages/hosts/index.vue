<script setup lang="ts">
import { useHostsStore, type Host } from '~/stores/hosts'
import { useStacksStore } from '~/stores/stacks'
import { useEventsStore } from '~/stores/events'
import { useUiStore } from '~/stores/ui'

const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()
const uiStore = useUiStore()

const sparklines = ref<Record<string, number[]>>({})
const inspectingHost = ref<Host | null>(null)

await Promise.all([
  hostsStore.load(),
  stacksStore.fetchAll(),
  eventsStore.load(200),
])

for (const host of hostsStore.hosts) {
  try {
    const { data, fetch } = useSparkline(host.id)
    await fetch('24h')
    if (data.value) sparklines.value[host.id] = data.value.buckets
  } catch {
    sparklines.value[host.id] = []
  }
}

const filteredHosts = computed(() => {
  const fleet = uiStore.activeFleet
  return fleet === 'all'
    ? hostsStore.hosts
    : hostsStore.hosts.filter((h) => h.fleet === fleet)
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

const router = useRouter()

function selectHost(host: Host) {
  // Hostname click → filtered Stacks list. Inspector opens via row ellipsis.
  router.push(`/stacks?host_id=${host.id}`)
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
  } else if (action === 'shell') {
    // 5e+: opens cmd pane terminal mode for the host's first service
  }
}
</script>

<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />
    <header class="flex items-center justify-between px-4 py-3 border-b border-iso-border-subtle">
      <div class="flex items-center gap-3">
        <h1 class="font-mono text-base text-iso-text-primary">Hosts</h1>
        <span class="text-xs text-iso-text-muted">
          {{ filteredHosts.length }} {{ filteredHosts.length === 1 ? 'host' : 'hosts' }}
        </span>
      </div>
      <AddHostButton />
    </header>
    <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
      <TableSkeleton v-if="!hostsStore.loaded" :rows="6" />
      <HostsTable
        v-else
        :hosts="filteredHosts"
        :sparklines="sparklines"
        :stack-counts="stackCounts"
        :latest-events="latestEvents"
        :selected-id="null"
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
  </div>
</template>
