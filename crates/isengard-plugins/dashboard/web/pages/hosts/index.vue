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
const fleetWeatherBuckets = ref<number[]>(new Array(24).fill(0))
const fleetWeatherRange = ref<'24h' | '7d'>('24h')
const inspectingHost = ref<Host | null>(null)

await Promise.all([
  hostsStore.load(),
  stacksStore.fetchAll(),
  eventsStore.load(200),
])

// Fetch sparklines for each host
for (const host of hostsStore.hosts) {
  try {
    const { data, fetch } = useSparkline(host.id)
    await fetch('24h')
    if (data.value) sparklines.value[host.id] = data.value.buckets
  } catch {
    sparklines.value[host.id] = []
  }
}

// Aggregate buckets across hosts for the FleetWeather strip
const aggregate = new Array(24).fill(0)
for (const buckets of Object.values(sparklines.value)) {
  buckets.forEach((v, i) => { aggregate[i] += v })
}
fleetWeatherBuckets.value = aggregate

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

const totalEvents = computed(() => fleetWeatherBuckets.value.reduce((a, b) => a + b, 0))

const router = useRouter()

function selectHost(host: Host) {
  // Hostname click → filtered Stacks list. Inspector opens via row ellipsis.
  router.push(`/stacks?host_id=${host.id}`)
}

function handleAction(action: 'force-update' | 'shell' | 'menu', host: Host) {
  if (action === 'menu') {
    inspectingHost.value = host
  } else if (action === 'force-update') {
    useHostActions().forceUpdate(host.id)
  } else if (action === 'shell') {
    // 5e: opens cmd pane terminal mode for the host's first service
  }
}
</script>

<template>
  <div class="h-screen flex flex-col">
    <TopBar />
    <FleetWeather
      :buckets="fleetWeatherBuckets"
      :range="fleetWeatherRange"
      :total-events="totalEvents"
      @range-change="(r) => fleetWeatherRange = r"
    />
    <div class="flex items-center justify-between px-4 py-3">
      <div class="text-sm text-iso-text-muted">
        {{ filteredHosts.length }} hosts
      </div>
      <AddHostButton @click="$router.push('/hosts?add=1')" />
    </div>
    <TableSkeleton v-if="!hostsStore.loaded" :rows="6" />
    <HostsTable
      v-else
      :hosts="filteredHosts"
      :sparklines="sparklines"
      :stack-counts="stackCounts"
      :latest-events="latestEvents"
      :selected-id="null"
      @select="selectHost"
      @action="handleAction"
    />
    <div class="mt-auto px-4 py-2 text-xs text-iso-text-faint border-t border-iso-border-subtle">
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">/</kbd> filter ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">j/k</kbd> move ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">Enter</kbd> open ·
      <kbd class="px-1.5 py-0.5 bg-iso-bg-elevated rounded">⌘K</kbd> cmd
    </div>

    <HostInspector
      v-if="inspectingHost"
      :host="inspectingHost"
      @close="inspectingHost = null"
      @changed="hostsStore.load()"
    />
  </div>
</template>
