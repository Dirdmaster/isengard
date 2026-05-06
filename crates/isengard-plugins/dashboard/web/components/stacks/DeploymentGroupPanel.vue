<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import {
  useDeploymentGroups,
  type DeploymentGroupDetailDto,
} from '~/composables/useDeploymentGroups'

// Phase 10c (T5 refs #50): in-flight multi-host deploy panel.
//
// Mounted on the stack detail page above the existing
// DeploymentInProgressPanel whenever a deployment group for this stack is
// pending or rolling. Shows progress, per-host strip, and an abort button.
// Single-host deploys never produce a group row, so this panel stays hidden.

const props = defineProps<{ stackId: string }>()
const stackIdRef = computed(() => props.stackId)

const { active, fetchDetail, abort, refresh } = useDeploymentGroups(stackIdRef)
const detail = ref<DeploymentGroupDetailDto | null>(null)
const aborting = ref(false)

const visibleGroup = computed(() => {
  if (!active.value.length) return null
  return [...active.value].sort(
    (a, b) => new Date(b.started_at).getTime() - new Date(a.started_at).getTime(),
  )[0]
})

async function loadDetail() {
  if (!visibleGroup.value) {
    detail.value = null
    return
  }
  try {
    detail.value = await fetchDetail(visibleGroup.value.id)
  } catch {
    detail.value = null
  }
}

watch(visibleGroup, loadDetail)
onMounted(loadDetail)

const TERMINAL_DEP_STATES = new Set(['done', 'aborted', 'failed'])

const totals = computed(() => {
  if (!detail.value) {
    return { done: 0, total: 0 }
  }
  const total = detail.value.deployments.length
  const done = detail.value.deployments.filter((d) =>
    TERMINAL_DEP_STATES.has(d.state),
  ).length
  return { done, total }
})

const progressPct = computed(() => {
  const { done, total } = totals.value
  if (total === 0) return 0
  return Math.round((done / total) * 100)
})

const stateClasses: Record<string, string> = {
  done: 'bg-iso-success/20 text-iso-success border-iso-success/40',
  failed: 'bg-iso-error/20 text-iso-error border-iso-error/40',
  aborted: 'bg-iso-warn/20 text-iso-warn border-iso-warn/40',
  pending: 'bg-iso-bg-elevated text-iso-text-muted border-iso-border-subtle',
  spinning_up: 'bg-iso-info/20 text-iso-info border-iso-info/40',
  switching: 'bg-iso-info/20 text-iso-info border-iso-info/40',
  draining: 'bg-iso-info/20 text-iso-info border-iso-info/40',
  destroying_blue: 'bg-iso-info/20 text-iso-info border-iso-info/40',
  recovering: 'bg-iso-warn/20 text-iso-warn border-iso-warn/40',
}

function chipClass(state: string) {
  return stateClasses[state] ?? 'bg-iso-bg-elevated text-iso-text-muted border-iso-border-subtle'
}

function shortHost(hostId: string) {
  // host_id is a 32-char hex string; show the first 8 chars to keep the strip
  // compact. Tooltip carries the full id.
  return hostId.slice(0, 8)
}

async function onAbort() {
  if (!visibleGroup.value || aborting.value) return
  if (!confirm('Abort this rolling deploy? Subsequent waves will be skipped.')) return
  aborting.value = true
  try {
    await abort(visibleGroup.value.id)
    await refresh()
    detail.value = null
  } catch (e) {
    useToast().error(`Abort failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    aborting.value = false
  }
}
</script>

<template>
  <div
    v-if="visibleGroup"
    class="rounded-md border border-iso-border-subtle bg-iso-bg-elevated/40 p-4 space-y-3"
  >
    <div class="flex items-start justify-between gap-3">
      <div>
        <div class="text-xs uppercase tracking-wider text-iso-text-faint">
          Multi-host deploy
        </div>
        <div class="font-mono text-iso-text-primary text-sm mt-0.5">
          {{ visibleGroup.service_name }}
          <span class="text-iso-text-muted text-xs ml-2">
            parallelism {{ visibleGroup.parallelism }}
          </span>
        </div>
      </div>
      <button
        class="px-3 py-1 rounded text-xs border border-iso-warn/40 text-iso-warn hover:bg-iso-warn/10 disabled:opacity-50"
        :disabled="aborting"
        @click="onAbort"
      >
        {{ aborting ? 'Aborting...' : 'Abort group' }}
      </button>
    </div>

    <!-- Progress bar -->
    <div>
      <div class="flex justify-between text-xs text-iso-text-muted mb-1">
        <span>{{ totals.done }} of {{ totals.total }} hosts done</span>
        <span>{{ progressPct }}%</span>
      </div>
      <div class="h-1.5 rounded-full bg-iso-border-subtle overflow-hidden">
        <div
          class="h-full bg-iso-info transition-all duration-300"
          :style="{ width: `${progressPct}%` }"
        />
      </div>
    </div>

    <!-- Per-host strip -->
    <div v-if="detail && detail.deployments.length" class="flex flex-wrap gap-2">
      <span
        v-for="d in detail.deployments"
        :key="d.id"
        class="inline-flex items-center gap-1 px-2 py-0.5 rounded-md border text-[11px] font-mono"
        :class="chipClass(d.state)"
        :title="`${d.host_id} - ${d.state}`"
      >
        {{ shortHost(d.host_id) }}
        <span class="opacity-70">{{ d.state }}</span>
      </span>
    </div>
  </div>
</template>
