<script setup lang="ts">
import { computed, ref } from 'vue'
import { useDeployments, type DeploymentDto } from '~/composables/useDeployments'
import { useDeploymentEvents } from '~/composables/useDeploymentEvents'
import EmptyState from '~/components/EmptyState.vue'

// Phase 10c (T4 refs #50): history tab polish.
// Filter chips for state, strategy, service + a time-range chip.
// Row expand reveals the full event timeline for the deployment.
// Group icon + tooltip shown when the row is part of a multi-host deploy.

const props = defineProps<{ stackId: string }>()
const stackIdRef = computed(() => props.stackId)

const { history, loading } = useDeployments(stackIdRef)

type StateFilter = 'all' | 'done' | 'failed' | 'aborted' | 'in-flight'
type StrategyFilter = 'all' | 'blue-green' | 'in-place'
type RangeFilter = '1h' | '24h' | '7d' | 'all'

const stateFilter = ref<StateFilter>('all')
const strategyFilter = ref<StrategyFilter>('all')
const serviceFilter = ref<string>('all')
const rangeFilter = ref<RangeFilter>('all')
const expanded = ref<string | null>(null)

const TERMINAL_STATES = new Set(['done', 'failed', 'aborted'])

const services = computed(() => {
  const set = new Set<string>()
  for (const d of history.value) set.add(d.service_name)
  return ['all', ...Array.from(set).sort()]
})

const filtered = computed<DeploymentDto[]>(() => {
  const cutoff = rangeCutoffMs(rangeFilter.value)
  return history.value.filter((d) => {
    if (stateFilter.value !== 'all') {
      if (stateFilter.value === 'in-flight') {
        if (TERMINAL_STATES.has(d.state)) return false
      } else if (d.state !== stateFilter.value) {
        return false
      }
    }
    if (strategyFilter.value !== 'all' && d.strategy !== strategyFilter.value) return false
    if (serviceFilter.value !== 'all' && d.service_name !== serviceFilter.value) return false
    if (cutoff !== null) {
      const ts = new Date(d.created_at).getTime()
      if (Number.isFinite(ts) && Date.now() - ts > cutoff) return false
    }
    return true
  })
})

function rangeCutoffMs(r: RangeFilter): number | null {
  switch (r) {
    case '1h':
      return 60 * 60 * 1000
    case '24h':
      return 24 * 60 * 60 * 1000
    case '7d':
      return 7 * 24 * 60 * 60 * 1000
    default:
      return null
  }
}

function toggleExpand(id: string) {
  expanded.value = expanded.value === id ? null : id
}

const expandedRef = computed(() => expanded.value)
const { events, loading: timelineLoading, error: timelineError } = useDeploymentEvents(expandedRef)

function fmtTime(iso: string) {
  try {
    const d = new Date(iso)
    return d.toLocaleString()
  } catch {
    return iso
  }
}

function durationLabel(d: { created_at: string; finished_at: string | null; updated_at: string }) {
  const start = new Date(d.created_at).getTime()
  const endIso = d.finished_at || d.updated_at
  const end = new Date(endIso).getTime()
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return '-'
  const ms = end - start
  if (ms < 1000) return `${ms}ms`
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const rs = s % 60
  return rs ? `${m}m ${rs}s` : `${m}m`
}

const stateClasses: Record<string, string> = {
  done: 'text-iso-success',
  failed: 'text-iso-error',
  aborted: 'text-iso-warn',
  pending: 'text-iso-text-muted',
  running: 'text-iso-info',
  switching: 'text-iso-info',
  draining: 'text-iso-info',
  spinning_up: 'text-iso-info',
  destroying_blue: 'text-iso-info',
  recovering: 'text-iso-warn',
}

const stateChips: { key: StateFilter; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'done', label: 'Done' },
  { key: 'failed', label: 'Failed' },
  { key: 'aborted', label: 'Aborted' },
  { key: 'in-flight', label: 'In-flight' },
]

const strategyChips: { key: StrategyFilter; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'blue-green', label: 'Blue-green' },
  { key: 'in-place', label: 'In-place' },
]

const rangeChips: { key: RangeFilter; label: string }[] = [
  { key: '1h', label: '1h' },
  { key: '24h', label: '24h' },
  { key: '7d', label: '7d' },
  { key: 'all', label: 'All' },
]

/**
 * Each deployment row may carry a `group_id` indicating it's one shard of a
 * multi-host rolling deploy. The DTO exposes it as `(d as any).group_id`
 * because Phase 10 Plan B's TypeScript surface doesn't list it yet; the
 * server includes it whenever the deployment was driven through the
 * orchestrator. Phase 10c (T4 refs #50).
 */
function groupIdOf(d: DeploymentDto): string | null {
  const v = (d as unknown as { group_id?: string | null }).group_id
  return typeof v === 'string' && v.length > 0 ? v : null
}

function chipClass(active: boolean) {
  return [
    'px-2 py-0.5 rounded text-[11px] border transition-colors',
    active
      ? 'border-iso-info text-iso-text-primary bg-iso-info/10'
      : 'border-iso-border-subtle text-iso-text-muted hover:text-iso-text-primary hover:border-iso-border',
  ]
}
</script>

<template>
  <div class="p-6 space-y-4">
    <!-- Filter chips ---------------------------------------------------- -->
    <div class="flex flex-wrap gap-3 text-xs">
      <div class="flex items-center gap-1">
        <span class="text-iso-text-faint pr-1 uppercase tracking-wider">State</span>
        <button
          v-for="c in stateChips"
          :key="c.key"
          :class="chipClass(stateFilter === c.key)"
          @click="stateFilter = c.key"
        >
          {{ c.label }}
        </button>
      </div>
      <div class="flex items-center gap-1">
        <span class="text-iso-text-faint pr-1 uppercase tracking-wider">Strategy</span>
        <button
          v-for="c in strategyChips"
          :key="c.key"
          :class="chipClass(strategyFilter === c.key)"
          @click="strategyFilter = c.key"
        >
          {{ c.label }}
        </button>
      </div>
      <div class="flex items-center gap-1">
        <span class="text-iso-text-faint pr-1 uppercase tracking-wider">Service</span>
        <button
          v-for="s in services"
          :key="s"
          :class="chipClass(serviceFilter === s)"
          @click="serviceFilter = s"
        >
          {{ s === 'all' ? 'All' : s }}
        </button>
      </div>
      <div class="flex items-center gap-1">
        <span class="text-iso-text-faint pr-1 uppercase tracking-wider">Range</span>
        <button
          v-for="c in rangeChips"
          :key="c.key"
          :class="chipClass(rangeFilter === c.key)"
          @click="rangeFilter = c.key"
        >
          {{ c.label }}
        </button>
      </div>
    </div>

    <!-- Body ------------------------------------------------------------ -->
    <div v-if="loading && history.length === 0" class="text-sm text-iso-text-muted">
      Loading deployment history...
    </div>

    <EmptyState
      v-else-if="history.length === 0"
      icon="history"
      title="No deployment history"
      description="Deployments for this stack will appear here once a deploy completes or aborts."
    />

    <div
      v-else-if="filtered.length === 0"
      class="text-sm text-iso-text-muted py-6 text-center"
    >
      No rows match the current filters.
    </div>

    <div v-else class="rounded-md border border-iso-border-subtle overflow-hidden">
      <table class="w-full text-sm">
        <thead class="bg-iso-bg-elevated text-iso-text-faint">
          <tr class="text-left text-xs uppercase tracking-wider">
            <th class="px-2 py-2 font-medium w-6"></th>
            <th class="px-3 py-2 font-medium">When</th>
            <th class="px-3 py-2 font-medium">Service</th>
            <th class="px-3 py-2 font-medium">Strategy</th>
            <th class="px-3 py-2 font-medium">State</th>
            <th class="px-3 py-2 font-medium">Duration</th>
            <th class="px-3 py-2 font-medium">Error</th>
          </tr>
        </thead>
        <tbody>
          <template v-for="d in filtered" :key="d.id">
            <tr
              class="border-t border-iso-border-subtle hover:bg-iso-bg-elevated/40 cursor-pointer"
              @click="toggleExpand(d.id)"
            >
              <td class="px-2 py-2 text-iso-text-faint text-center">
                <span class="font-mono">{{ expanded === d.id ? '-' : '+' }}</span>
              </td>
              <td class="px-3 py-2 font-mono text-xs text-iso-text-muted whitespace-nowrap">
                {{ fmtTime(d.created_at) }}
              </td>
              <td class="px-3 py-2 font-mono text-iso-text-primary">
                <span>{{ d.service_name }}</span>
                <span
                  v-if="groupIdOf(d)"
                  class="ml-2 inline-flex items-center px-1.5 py-0.5 rounded bg-iso-info/10 text-iso-info text-[10px] font-medium"
                  :title="`Part of multi-host deploy (group ${groupIdOf(d)})`"
                  @click.stop
                >
                  group
                </span>
              </td>
              <td class="px-3 py-2 text-iso-text-muted">{{ d.strategy }}</td>
              <td class="px-3 py-2 font-medium" :class="stateClasses[d.state] ?? 'text-iso-text-muted'">
                {{ d.state }}
              </td>
              <td class="px-3 py-2 text-iso-text-muted">{{ durationLabel(d) }}</td>
              <td class="px-3 py-2 text-iso-error text-xs truncate max-w-xs" :title="d.error ?? ''">
                {{ d.error || '-' }}
              </td>
            </tr>
            <tr v-if="expanded === d.id" class="bg-iso-bg-elevated/40">
              <td colspan="7" class="px-6 py-4">
                <div class="text-xs uppercase tracking-wider text-iso-text-faint mb-2">
                  Event timeline
                </div>
                <div v-if="timelineLoading" class="text-iso-text-muted text-xs">
                  Loading events...
                </div>
                <div v-else-if="timelineError" class="text-iso-error text-xs">
                  {{ timelineError }}
                </div>
                <div v-else-if="events.length === 0" class="text-iso-text-faint text-xs">
                  No events recorded for this deployment.
                </div>
                <ul v-else class="space-y-1">
                  <li v-for="e in events" :key="e.id" class="text-xs flex gap-3">
                    <span class="font-mono text-iso-text-faint whitespace-nowrap">
                      {{ fmtTime(e.occurred_at) }}
                    </span>
                    <span class="font-mono text-iso-text-primary">{{ e.kind }}</span>
                    <span class="text-iso-text-muted truncate">
                      {{ e.summary }}<span v-if="e.error">: {{ e.error }}</span>
                    </span>
                  </li>
                </ul>
              </td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>
  </div>
</template>

