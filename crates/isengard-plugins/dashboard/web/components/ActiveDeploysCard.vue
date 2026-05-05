<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

/**
 * Active deploys card — header + per-deploy progress bar.
 *
 * Per `design/concepts/home/v1.html` right column. Standalone fetch (not tied
 * to a specific stack like `useDeployments`), since home shows all active.
 *
 * Progress is heuristic: we don't have a true % from the backend. blue-green
 * lifecycle states map roughly to:
 *   spinning_up   → 20%
 *   healthcheck   → 40%
 *   switching     → 60%
 *   draining      → 80%
 *   complete      → 100%
 */

const api = useApi()
const stacksStore = useStacksStore()

const active = ref<DeploymentDto[]>([])

onMounted(async () => {
  try {
    active.value = await api.get<DeploymentDto[]>('/deployments', { state: 'active' })
  } catch {
    active.value = []
  }
})

const count = computed(() => active.value.length)

function progressOf(d: DeploymentDto): number {
  if (d.drained_at) return 100
  if (d.switched_at) return 60
  if (d.healthcheck_passed_at) return 40
  return 20
}

function phaseLabel(d: DeploymentDto): string {
  if (d.drained_at) return 'draining'
  if (d.switched_at) return 'switching'
  if (d.healthcheck_passed_at) return 'health-checking'
  return 'spinning up'
}

function stackName(stackId: number | string): string {
  const id = String(stackId)
  return stacksStore.items.find(s => s.id === id)?.name ?? `stack ${id}`
}

function shortDigest(d: string | null): string {
  if (!d) return ''
  // sha256:abc123...  → ":abc1234"
  const m = d.match(/sha256:([a-f0-9]{7})/)
  if (m) return m[1]
  return d.slice(0, 7)
}
</script>

<template>
  <div class="rounded-iso-lg border bg-iso-bg-elevated flex flex-col overflow-hidden"
       :class="count > 0 ? 'border-iso-info' : 'border-iso-border-subtle'">
    <div class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between">
      <span class="text-iso-xs font-semibold text-iso-text-primary">Active deploys</span>
      <span class="text-[11px] text-iso-text-muted">{{ count === 0 ? 'idle' : `${count} in progress` }}</span>
    </div>

    <div v-if="count === 0" class="p-4 text-iso-xs text-iso-text-faint">
      No deploys running. New rollouts appear here as they switch traffic.
    </div>

    <div
      v-for="d in active"
      :key="d.id"
      class="p-4 flex flex-col gap-2.5 border-b border-iso-border-subtle last:border-b-0"
    >
      <div class="flex items-center justify-between gap-3">
        <span class="text-iso-sm font-medium text-iso-text-primary truncate">{{ d.service_name }}</span>
        <span class="font-mono text-[11px] text-iso-text-muted shrink-0">
          {{ shortDigest(d.blue_digest) }} → {{ shortDigest(d.green_digest) }}
        </span>
      </div>
      <div class="h-1.5 rounded-full bg-iso-bg-base overflow-hidden">
        <div
          class="h-full rounded-full bg-iso-info transition-all duration-500"
          :style="{ width: `${progressOf(d)}%` }"
        ></div>
      </div>
      <div class="flex items-center justify-between text-[11px]">
        <span class="text-iso-text-muted truncate">{{ phaseLabel(d) }} · {{ stackName(d.stack_id) }}</span>
        <span class="font-mono text-iso-info shrink-0">{{ progressOf(d) }}%</span>
      </div>
    </div>
  </div>
</template>
