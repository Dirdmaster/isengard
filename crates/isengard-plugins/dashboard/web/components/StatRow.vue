<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import type { DeploymentDto } from '~/composables/useDeployments'

/**
 * Home page stat row: 4 cells (HOSTS, STACKS, APPROVALS, DEPLOYS).
 *
 * Source: `design/concepts/home/v1.html`. The 10-second answer to "is anything
 * happening?". Each cell has a status dot, uppercase label, big mono number,
 * a secondary count, and a status footer line. Warn/info cells use soft
 * tinted backgrounds.
 *
 * Approvals count comes from `usePendingApprovalsCount` (already polled once
 * globally for the TopBar badge: zero extra fetches). Active deploys are a
 * best-effort fetch in onMounted. Hosts and stacks read from their stores.
 */

const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const eventsStore = useEventsStore()
const api = useApi()
const router = useRouter()
const { count: pendingApprovals } = usePendingApprovalsCount()

const activeDeploys = ref<DeploymentDto[]>([])

onMounted(async () => {
  // Best-effort: keep StatRow rendering even if the deployments call fails.
  try {
    activeDeploys.value = await api.get<DeploymentDto[]>('/deployments', { state: 'active' })
  } catch {
    activeDeploys.value = []
  }
})

// ─── HOSTS ────────────────────────────────────────────────────────────────
const hostsTotal = computed(() => hostsStore.hosts.length)

const hostsUp = computed(() => {
  const cutoff = Date.now() - 5 * 60 * 1000
  return hostsStore.hosts.filter((h) => {
    if (!h.last_seen_at) return false
    return new Date(h.last_seen_at).getTime() >= cutoff
  }).length
})

const hostsAllHealthy = computed(() => hostsTotal.value > 0 && hostsUp.value === hostsTotal.value)

// Hosts enrolled in the last 24h: source for the "+1 today" delta.
const hostsAddedToday = computed(() => {
  const cutoff = Date.now() - 24 * 60 * 60 * 1000
  return hostsStore.hosts.filter((h) => {
    if (!h.enrolled_at) return false
    return new Date(h.enrolled_at).getTime() >= cutoff
  }).length
})

// ─── STACKS ───────────────────────────────────────────────────────────────
const stacksTotal = computed(() => stacksStore.items.length)

const stacksDegraded = computed(() => {
  // Heuristic until a real stack-health surface exists: any stack whose host
  // emitted an `update.failed` recently is treated as degraded.
  const failedHostIds = new Set(
    eventsStore.events
      .filter((e) => e.kind === 'update.failed' && e.host_id)
      .slice(0, 50)
      .map((e) => e.host_id as string),
  )
  return stacksStore.items.filter((s) => failedHostIds.has(s.host_id)).length
})

const stacksHealthy = computed(() => stacksTotal.value - stacksDegraded.value)

// ─── DEPLOYS ──────────────────────────────────────────────────────────────
const deploysInProgress = computed(() => activeDeploys.value.length)

const primaryDeploy = computed(() => activeDeploys.value[0] ?? null)

// blue-green lifecycle as a rough %: spinning_up / healthcheck / switching /
// drained map to four bands. Cheap proxy: backend doesn't expose a real %.
const primaryDeployProgress = computed(() => {
  const d = primaryDeploy.value
  if (!d) return null
  if (d.drained_at) return 100
  if (d.switched_at) return 60
  if (d.healthcheck_passed_at) return 40
  return 20
})

// 24h deploys count for the DEPLOYS cell footer: every `update.success` or
// `deploy.switching` event in the last 24h, deduped by container.
const deploys24h = computed(() => {
  const cutoff = Date.now() - 24 * 60 * 60 * 1000
  const seen = new Set<string>()
  for (const e of eventsStore.events) {
    if (!e.occurred_at) continue
    if (new Date(e.occurred_at).getTime() < cutoff) continue
    const looksLikeDeploy =
      e.kind === 'update.success' ||
      e.kind.startsWith('deploy.switching') ||
      e.kind === 'deployment.switched'
    if (!looksLikeDeploy) continue
    const key = `${e.host_id ?? ''}:${e.container_name ?? ''}:${e.kind}`
    seen.add(key)
  }
  return seen.size
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
        <span class="text-2xl font-semibold font-mono text-iso-text-primary">{{ hostsTotal }}</span>
        <span class="text-iso-xs text-iso-text-muted" v-if="hostsAddedToday > 0">
          ↑{{ hostsAddedToday }} today
        </span>
        <span class="text-iso-xs text-iso-text-muted" v-else>
          / {{ hostsUp }} up
        </span>
      </div>
      <span
        class="text-[11px]"
        :class="hostsAllHealthy ? 'text-iso-success' : 'text-iso-text-muted'"
      >
        <template v-if="hostsTotal === 0">no hosts enrolled</template>
        <template v-else-if="hostsAllHealthy">all healthy</template>
        <template v-else>{{ hostsTotal - hostsUp }} stale</template>
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
        <span
          v-if="deploysInProgress > 0"
          class="px-1.5 py-px rounded-iso-sm bg-iso-info-soft font-mono text-[10px] text-iso-info"
        >{{ deploysInProgress }} deploying</span>
      </div>
      <span class="text-[11px] text-iso-text-muted">
        <template v-if="stacksTotal === 0">none yet</template>
        <template v-else-if="stacksDegraded === 0">{{ stacksHealthy }} healthy</template>
        <template v-else>{{ stacksHealthy }} healthy · {{ stacksDegraded }} degraded</template>
      </span>
    </button>

    <!-- APPROVALS -->
    <button
      type="button"
      class="p-4 rounded-iso-lg flex flex-col gap-1.5 text-left transition-colors"
      :class="pendingApprovals > 0
        ? 'bg-iso-warn-soft border border-iso-warn hover:opacity-90'
        : 'bg-iso-bg-elevated border border-iso-border-subtle hover:border-iso-border-strong'"
      @click="go('/approvals')"
    >
      <div class="flex items-center gap-2">
        <div
          class="w-2 h-2 rounded-full"
          :class="pendingApprovals > 0 ? 'bg-iso-warn' : 'bg-iso-success'"
        ></div>
        <span
          class="text-[11px] font-medium tracking-wide"
          :class="pendingApprovals > 0 ? 'text-iso-warn' : 'text-iso-text-muted'"
        >APPROVALS</span>
      </div>
      <div class="flex items-baseline gap-2">
        <span
          class="text-2xl font-semibold font-mono"
          :class="pendingApprovals > 0 ? 'text-iso-warn' : 'text-iso-text-primary'"
        >{{ pendingApprovals }}</span>
        <span class="text-iso-xs text-iso-text-muted">pending</span>
      </div>
      <span
        class="text-[11px]"
        :class="pendingApprovals > 0 ? 'text-iso-warn' : 'text-iso-text-muted'"
      >
        <template v-if="pendingApprovals > 0">Review now →</template>
        <template v-else>nothing waiting</template>
      </span>
    </button>

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
        <template v-else-if="deploys24h > 0">
          {{ deploys24h }} in last 24h
        </template>
        <template v-else>no active deploys</template>
      </span>
    </button>
  </div>
</template>
