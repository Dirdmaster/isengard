<template>
  <div class="m-3 p-4 rounded-lg bg-iso-bg-elevated border border-iso-border-subtle">
    <!-- Header row -->
    <div class="flex items-center gap-3.5">
      <div
        class="w-6 h-6 rounded-full flex items-center justify-center shrink-0"
        :style="{ backgroundColor: iconBg }"
      >
        <Icon :name="iconName" class="w-3.5 h-3.5" :style="{ color: iconColor }" />
      </div>

      <div class="min-w-0">
        <h4 class="font-semibold text-iso-text-primary">Fleet · <span class="font-mono">{{ fleet.name }}</span></h4>
        <div class="flex gap-3 mt-1 text-sm flex-wrap">
          <span v-if="failedCount > 0" class="text-iso-error">{{ failedCount }} failed</span>
          <span v-if="updatingCount > 0" class="text-iso-warn">{{ updatingCount }} updating</span>
          <span v-if="failedCount === 0 && updatingCount === 0" class="text-iso-success">all clear</span>
          <span class="text-iso-text-muted">·</span>
          <span class="text-iso-text-muted">{{ fleet.host_count }} {{ fleet.host_count === 1 ? 'host' : 'hosts' }}</span>
          <template v-if="lastCycleAgo">
            <span class="text-iso-text-muted">·</span>
            <span class="text-iso-text-muted">last activity {{ lastCycleAgo }}</span>
          </template>
        </div>
      </div>
    </div>

    <div v-if="issues.length > 0" class="mt-4">
      <div class="text-xs uppercase tracking-wider text-iso-text-faint mb-2">{{ issues.length }} need attention</div>
      <div class="space-y-1">
        <div v-for="issue in issues" :key="issue.id" class="flex items-center gap-2.5 py-1.5 border-b border-iso-border-subtle last:border-b-0 text-sm">
          <div class="w-2 h-2 rounded-full shrink-0" :style="{ backgroundColor: stateColor(issue.state), boxShadow: `0 0 6px ${stateColor(issue.state)}66` }"></div>
          <span class="text-iso-text-primary truncate">
            {{ issue.container_name }}
            <span v-if="issue.host_name" class="text-iso-text-faint font-mono text-xs">on {{ issue.host_name }}</span>
          </span>
          <div class="flex-1"></div>
          <span class="text-iso-text-muted font-mono text-xs truncate">{{ issue.detail }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { Fleet } from '~/stores/fleets'

const props = defineProps<{
  fleet: Fleet
}>()

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()

// Issues = recent events with a problematic kind, filtered to this fleet's hosts.
const fleetHostIds = computed(() =>
  new Set(hostsStore.hosts.filter(h => '' === props.fleet.name).map(h => h.id))
)

function hostnameOf(hostId: string | null | undefined): string {
  if (!hostId) return ''
  return hostsStore.hosts.find(h => h.id === hostId)?.hostname ?? hostId.slice(0, 6)
}

const issues = computed(() => {
  return eventsStore.events
    .filter(e => e.host_id && fleetHostIds.value.has(e.host_id))
    .filter(e => ['update.failed', 'update.pulling', 'agent.disconnect_long'].includes(e.kind))
    .slice(0, 5)
    .map(e => ({
      id: e.id,
      container_name: e.container_name ?? e.kind,
      host_name: hostnameOf(e.host_id),
      detail: e.summary,
      state: kindToState(e.kind),
    }))
})

function kindToState(kind: string): 'success'|'warn'|'error'|'info' {
  if (kind === 'update.failed') return 'error'
  if (kind === 'update.pulling') return 'warn'
  if (kind === 'agent.disconnect_long') return 'info'
  return 'success'
}

const updatingCount = computed(() => issues.value.filter(i => i.state === 'warn').length)
const failedCount = computed(() => issues.value.filter(i => i.state === 'error').length)

const iconName = computed(() => {
  if (failedCount.value > 0) return 'lucide:triangle-alert'
  if (updatingCount.value > 0) return 'lucide:loader'
  return 'lucide:check'
})

const iconBg = computed(() => {
  if (failedCount.value > 0) return '#f8717126'
  if (updatingCount.value > 0) return '#fbbf2426'
  return '#4ade8026'
})

const iconColor = computed(() => {
  if (failedCount.value > 0) return '#f87171'
  if (updatingCount.value > 0) return '#fbbf24'
  return '#4ade80'
})

// Derive last activity from the most recent event for this fleet.
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

function stateColor(s: string) {
  const map: Record<string, string> = {
    success: '#4ade80',
    warn: '#fbbf24',
    error: '#f87171',
    info: '#c084fc',
  }
  return map[s] ?? '#94a3b8'
}
</script>
