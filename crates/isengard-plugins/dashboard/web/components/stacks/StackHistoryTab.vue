<script setup lang="ts">
import { computed } from 'vue'
import { useDeployments } from '~/composables/useDeployments'
import EmptyState from '~/components/EmptyState.vue'

const props = defineProps<{ stackId: string }>()
const stackIdRef = computed(() => props.stackId)

const { history, loading } = useDeployments(stackIdRef)

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
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return '—'
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
}
</script>

<template>
  <div class="p-6">
    <div v-if="loading && history.length === 0" class="text-sm text-iso-text-muted">
      Loading deployment history…
    </div>

    <EmptyState
      v-else-if="history.length === 0"
      icon="history"
      title="No deployment history"
      description="Deployments for this stack will appear here once a deploy completes or aborts."
    />

    <div v-else class="rounded-md border border-iso-border-subtle overflow-hidden">
      <table class="w-full text-sm">
        <thead class="bg-iso-bg-elevated text-iso-text-faint">
          <tr class="text-left text-xs uppercase tracking-wider">
            <th class="px-3 py-2 font-medium">When</th>
            <th class="px-3 py-2 font-medium">Service</th>
            <th class="px-3 py-2 font-medium">Strategy</th>
            <th class="px-3 py-2 font-medium">State</th>
            <th class="px-3 py-2 font-medium">Duration</th>
            <th class="px-3 py-2 font-medium">Error</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="d in history"
            :key="d.id"
            class="border-t border-iso-border-subtle hover:bg-iso-bg-elevated/40"
          >
            <td class="px-3 py-2 font-mono text-xs text-iso-text-muted whitespace-nowrap">
              {{ fmtTime(d.created_at) }}
            </td>
            <td class="px-3 py-2 font-mono text-iso-text-primary">{{ d.service_name }}</td>
            <td class="px-3 py-2 text-iso-text-muted">{{ d.strategy }}</td>
            <td class="px-3 py-2 font-medium" :class="stateClasses[d.state] ?? 'text-iso-text-muted'">
              {{ d.state }}
            </td>
            <td class="px-3 py-2 text-iso-text-muted">{{ durationLabel(d) }}</td>
            <td class="px-3 py-2 text-iso-error text-xs truncate max-w-xs" :title="d.error ?? ''">
              {{ d.error || '—' }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>
