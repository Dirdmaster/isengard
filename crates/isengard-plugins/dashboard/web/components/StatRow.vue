<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import type { DeploymentDto } from '~/composables/useDeployments'

/**
 * Home page stat row: 4 cells — HOSTS / STACKS / APPROVALS / DEPLOYS.
 *
 * Per `design/concepts/home/v1.html` — the 10-second answer to "is anything
 * happening?". Each cell has a status dot, uppercase label, big mono number,
 * secondary count, status footer. Warn/info cells have soft backgrounds and
 * tinted borders.
 *
 * Approvals stays disabled until Phase 9 ships (see `design/pages/approvals.md`).
 */

const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()
const api = useApi()
const router = useRouter()

const activeDeploys = ref<DeploymentDto[]>([])

onMounted(async () => {
  // Best-effort: keep StatRow rendering even if the deployments call fails.
  try {
    activeDeploys.value = await api.get<DeploymentDto[]>('/deployments', { state: 'active' })
  } catch {
    activeDeploys.value = []
  }
})

// Hosts up: hosts with last_seen_at within the last 5 minutes.
const hostsUp = computed(() => {
  const cutoff = Date.now() - 5 * 60 * 1000
  return hostsStore.hosts.filter((h) => {
    if (!h.last_seen_at) return false
    return new Date(h.last_seen_at).getTime() >= cutoff
  }).length
})

const hostsTotal = computed(() => hostsStore.hosts.length)
const hostsAllHealthy = computed(() => hostsTotal.value > 0 && hostsUp.value === hostsTotal.value)

// Stacks: count + degraded heuristic from recent failure events.
const stacksTotal = computed(() => stacksStore.items.length)

const stacksDegraded = computed(() => {
  const failedHostIds = new Set(
    eventsStore.events
      .filter((e) => e.kind === 'update.failed' && e.host_id)
      .slice(0, 50)
      .map((e) => e.host_id as string),
  )
  return stacksStore.items.filter((s) => failedHostIds.has(s.host_id)).length
})

const stacksHealthy = computed(() => stacksTotal.value - stacksDegraded.value)

const deploysInProgress = computed(() => activeDeploys.value.length)

// Concept shows "web-app · 60%" — pick the first active deploy and its
// switching progress. Heuristic: switched_at && !drained_at => switching.
const primaryDeploy = computed(() => activeDeploys.value[0] ?? null)

const primaryDeployProgress = computed(() => {
  const d = primaryDeploy.value
  if (!d) return null
  // Cheap proxy for progress — we don't have a true % from the backend.
  if (d.drained_at) return 100
  if (d.switched_at) return 60
  if (d.healthcheck_passed_at) return 40
  return 20
})

function go(path: string) {
  router.push(path)
}
</script>

<template>
  <div class="grid grid-cols-2 md:grid-cols-4 gap-3.5 shrink-0">
    <!-- HOSTS -->
    <button
      type="button"
      class="p-4 rounded-iso-lg bg-iso-bg-elevated border border-iso-border-subtle flex flex-col gap-1.5 text-left hover:border-iso-border-strong transition-colors"
      @click="go('/hosts')"
    >
      <div class="flex items-center gap-2">
        <div
          class="w-2 h-2 rounded-full"
          :class="hostsAllHealthy ? 'bg-iso-success' : hostsTotal === 0 ? 'bg-iso-text-faint' : 'bg-iso-warn'"
        ></div>
        <span class="text-[11px] font-medium text-iso-text-muted tracking-wide">HOSTS</span>
      </div>
      <div class="flex items-baseline gap-2">
        <span class="text-2xl font-semibold font-mono text-iso-text-primary">{{ hostsUp }}</span>
        <span class="text-iso-xs text-iso-text-muted">/ {{ hostsTotal }} up</span>
      </div>
      <span
        class="text-[11px]"
        :class="hostsAllHealthy ? 'text-iso-success' : 'text-iso-text-muted'"
      >
        {{ hostsTotal === 0 ? 'no hosts enrolled' : hostsAllHealthy ? 'all healthy' : `${hostsTotal - hostsUp} stale` }}
      </span>
    </button>

    <!-- STACKS -->
    <button
      type="button"
      class="p-4 rounded-iso-lg bg-iso-bg-elevated border border-iso-border-subtle flex flex-col gap-1.5 text-left hover:border-iso-border-strong transition-colors"
      @click="go('/stacks')"
    >
      <div class="flex items-center gap-2">
        <div
          class="w-2 h-2 rounded-full"
          :class="stacksDegraded > 0 ? 'bg-iso-warn' : 'bg-iso-success'"
        ></div>
        <span class="text-[11px] font-medium text-iso-text-muted tracking-wide">STACKS</span>
      </div>
      <div class="flex items-baseline gap-2">
        <span class="text-2xl font-semibold font-mono text-iso-text-primary">{{ stacksTotal }}</span>
        <span class="text-iso-xs text-iso-text-muted">tracked</span>
      </div>
      <span class="text-[11px] text-iso-text-muted">
        <template v-if="stacksTotal === 0">none yet</template>
        <template v-else-if="stacksDegraded === 0">{{ stacksHealthy }} healthy</template>
        <template v-else>{{ stacksHealthy }} healthy · {{ stacksDegraded }} degraded</template>
      </span>
    </button>

    <!-- APPROVALS — Phase 9 deferred -->
    <div
      class="p-4 rounded-iso-lg bg-iso-bg-elevated border border-iso-border-subtle flex flex-col gap-1.5 text-left opacity-60"
      title="Approvals coming with Phase 9 (update policies)"
    >
      <div class="flex items-center gap-2">
        <div class="w-2 h-2 rounded-full bg-iso-text-faint"></div>
        <span class="text-[11px] font-medium text-iso-text-muted tracking-wide">APPROVALS</span>
      </div>
      <div class="flex items-baseline gap-2">
        <span class="text-2xl font-semibold font-mono text-iso-text-faint">—</span>
        <span class="text-iso-xs text-iso-text-muted">pending</span>
      </div>
      <span class="text-[11px] text-iso-text-faint">phase 9</span>
    </div>

    <!-- DEPLOYS -->
    <button
      type="button"
      class="p-4 rounded-iso-lg flex flex-col gap-1.5 text-left transition-colors"
      :class="deploysInProgress > 0
        ? 'bg-iso-info-soft border border-iso-info hover:opacity-90'
        : 'bg-iso-bg-elevated border border-iso-border-subtle hover:border-iso-border-strong'"
      @click="go('/stacks')"
    >
      <div class="flex items-center gap-2">
        <div
          class="w-2 h-2 rounded-full"
          :class="deploysInProgress > 0 ? 'bg-iso-info' : 'bg-iso-success'"
        ></div>
        <span
          class="text-[11px] font-medium tracking-wide"
          :class="deploysInProgress > 0 ? 'text-iso-info' : 'text-iso-text-muted'"
        >DEPLOYS</span>
      </div>
      <div class="flex items-baseline gap-2">
        <span
          class="text-2xl font-semibold font-mono"
          :class="deploysInProgress > 0 ? 'text-iso-info' : 'text-iso-text-primary'"
        >{{ deploysInProgress }}</span>
        <span class="text-iso-xs text-iso-text-muted">{{ deploysInProgress === 0 ? 'idle' : 'in progress' }}</span>
      </div>
      <span
        class="text-[11px]"
        :class="deploysInProgress > 0 ? 'text-iso-info' : 'text-iso-text-muted'"
      >
        <template v-if="primaryDeploy && primaryDeployProgress !== null">
          {{ primaryDeploy.service_name }} · {{ primaryDeployProgress }}%
        </template>
        <template v-else>no active deploys</template>
      </span>
    </button>
  </div>
</template>
