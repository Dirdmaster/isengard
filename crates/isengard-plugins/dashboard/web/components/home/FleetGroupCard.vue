<template>
  <div class="rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle px-4 py-3.5 flex flex-col gap-3.5">
    <!-- Top row: status icon + title/stats + spacer + mini-grid -->
    <div class="flex items-center gap-3.5">
      <div
        class="w-6 h-6 rounded-full flex items-center justify-center shrink-0"
        :style="{ backgroundColor: iconBg }"
      >
        <Icon :name="iconName" class="w-3.5 h-3.5" :style="{ color: iconColor }" />
      </div>

      <div class="flex flex-col gap-1 min-w-0">
        <h4 class="text-sm font-semibold text-iso-text-primary">
          Fleet · <span class="font-mono">{{ fleet.name }}</span>
        </h4>
        <div class="flex items-center gap-3 text-xs flex-wrap">
          <span v-if="counts.healthy > 0" class="text-iso-success">
            {{ counts.healthy }} healthy
          </span>
          <span v-if="counts.updating > 0" class="text-iso-warn">
            {{ counts.updating }} updating
          </span>
          <span v-if="counts.failed > 0" class="text-iso-error">
            {{ counts.failed }} failed
          </span>
          <span v-if="counts.healthy + counts.updating + counts.failed === 0" class="text-iso-text-muted">
            no containers tracked
          </span>
        </div>
        <span class="text-[11px] text-iso-text-muted">{{ subline }}</span>
      </div>

      <div class="flex-1"></div>

      <!-- Mini grid: one rounded square per container, color-coded -->
      <div v-if="cells.length > 0" class="flex items-center gap-[3px] shrink-0">
        <div
          v-for="(cell, i) in cells"
          :key="i"
          class="w-[14px] h-[14px] rounded-[2px]"
          :style="{ backgroundColor: cell }"
        ></div>
      </div>
    </div>

    <!-- Needs attention sub-section -->
    <div v-if="issues.length > 0" class="flex flex-col gap-0">
      <div class="text-[10px] tracking-[0.12em] font-medium text-iso-text-faint uppercase">
        {{ issues.length }} need attention
      </div>
      <NeedsAttentionRow
        v-for="issue in issues"
        :key="issue.id"
        :issue="issue"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { Fleet } from '~/stores/fleets'
import type { EventRow } from '~/stores/events'

/**
 * Fleet group card · matches Pencil k3bOyE State strip.
 *
 * Shows a per-fleet rollup: status icon, title, inline counts, mini-grid of
 * container state cells, and a collapsible "needs attention" sub-section.
 *
 * The status counts are derived heuristically from the recent event stream:
 *  - failed   = service has a recent `update.failed` / `routing.degraded`
 *  - updating = service has a recent `update.pulling` and no failure since
 *  - healthy  = remaining stacks on this fleet's hosts
 *
 * "Last cycle" is the most recent event timestamp for any host in the fleet.
 */

const props = defineProps<{
  fleet: Fleet
}>()

const eventsStore = useEventsStore()
const hostsStore  = useHostsStore()
const stacksStore = useStacksStore()

const fleetHosts = computed(() =>
  hostsStore.hosts.filter(h => h.fleet === props.fleet.name),
)

const fleetHostIds = computed(() => new Set(fleetHosts.value.map(h => h.id)))

function hostnameOf(hostId: string | null | undefined): string {
  if (!hostId) return ''
  return hostsStore.hosts.find(h => h.id === hostId)?.hostname ?? hostId.slice(0, 6)
}

// Per-service rollup keyed by `host_id::container_name`.
type ServiceState = 'healthy' | 'updating' | 'failed'

interface ServiceRollup {
  key: string
  host_id: string
  container_name: string
  state: ServiceState
  latestFailure?: EventRow
}

const services = computed<ServiceRollup[]>(() => {
  const byKey = new Map<string, ServiceRollup>()

  // Seed from stacks: every tracked stack on a fleet host counts as healthy.
  for (const s of stacksStore.items) {
    if (!fleetHostIds.value.has(s.host_id)) continue
    const k = `${s.host_id}::${s.name}`
    if (!byKey.has(k)) {
      byKey.set(k, {
        key: k,
        host_id: s.host_id,
        container_name: s.name,
        state: 'healthy',
      })
    }
  }

  // Walk recent events (newest first). For each service we encounter, the
  // *first* event we see wins — so the most recent state is what shows.
  const seen = new Set<string>()
  for (const e of eventsStore.events) {
    if (!e.host_id || !e.container_name) continue
    if (!fleetHostIds.value.has(e.host_id)) continue
    const k = `${e.host_id}::${e.container_name}`
    if (seen.has(k)) continue

    let state: ServiceState | null = null
    if (e.kind === 'update.failed' || e.kind === 'routing.degraded' || e.kind === 'healthcheck.failed') {
      state = 'failed'
    } else if (e.kind === 'update.pulling' || e.kind === 'update.pending_approval') {
      state = 'updating'
    } else if (e.kind === 'update.success' || e.kind === 'routing.healthy') {
      state = 'healthy'
    }
    if (state === null) continue
    seen.add(k)

    const entry = byKey.get(k) ?? {
      key: k,
      host_id: e.host_id,
      container_name: e.container_name,
      state,
    }
    entry.state = state
    if (state === 'failed') entry.latestFailure = e
    byKey.set(k, entry)
  }

  return Array.from(byKey.values())
})

const counts = computed(() => {
  const out = { healthy: 0, updating: 0, failed: 0 }
  for (const s of services.value) out[s.state]++
  return out
})

// Mini-grid cells: one per service, ordered failed → updating → healthy.
const cells = computed(() => {
  const order = [...services.value].sort((a, b) => stateRank(a.state) - stateRank(b.state))
  return order.map(s => stateColor(s.state))
})

function stateRank(s: ServiceState): number {
  if (s === 'failed') return 0
  if (s === 'updating') return 1
  return 2
}

function stateColor(s: ServiceState | string): string {
  if (s === 'failed') return 'var(--iso-accent-error)'
  if (s === 'updating') return 'var(--iso-accent-warn)'
  return 'var(--iso-accent-success)'
}

// Status icon + tone reflect the worst non-healthy state.
const overallState = computed<ServiceState>(() => {
  if (counts.value.failed > 0) return 'failed'
  if (counts.value.updating > 0) return 'updating'
  return 'healthy'
})

const iconName = computed(() => {
  if (overallState.value === 'failed') return 'lucide:triangle-alert'
  if (overallState.value === 'updating') return 'lucide:loader'
  return 'lucide:check'
})

const iconBg = computed(() => {
  if (overallState.value === 'failed') return '#f8717126'
  if (overallState.value === 'updating') return '#fbbf2426'
  return '#4ade8026'
})

const iconColor = computed(() => {
  if (overallState.value === 'failed') return '#f87171'
  if (overallState.value === 'updating') return '#fbbf24'
  return '#4ade80'
})

const lastCycleAgo = computed(() => {
  const last = eventsStore.events.find(e => e.host_id && fleetHostIds.value.has(e.host_id))
  if (!last) return null
  const ms = Date.now() - new Date(last.occurred_at).getTime()
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.floor(h / 24)
  return `${d}d ago`
})

const subline = computed(() => {
  const containers = services.value.length
  const hosts = fleetHosts.value.length
  const cont = `${containers} ${containers === 1 ? 'container' : 'containers'}`
  const hostStr = `${hosts} ${hosts === 1 ? 'host' : 'hosts'}`
  const cycle = lastCycleAgo.value ? ` · last cycle ${lastCycleAgo.value}` : ''
  return `${cont} across ${hostStr}${cycle}`
})

// Issues for the needs-attention sub-section: every failed/updating service.
const issues = computed(() => {
  return services.value
    .filter(s => s.state !== 'healthy')
    .slice(0, 5)
    .map(s => {
      const ev = s.latestFailure
      const hostName = hostnameOf(s.host_id)
      let detail = ''
      if (s.state === 'failed') {
        detail = ev?.summary ?? 'recent failure'
      } else if (s.state === 'updating') {
        detail = ev?.image ? `pulling ${ev.image}` : 'updating'
      }
      return {
        id: s.key,
        container_name: s.container_name,
        host_name: hostName,
        detail,
        state: s.state,
      }
    })
})
</script>
