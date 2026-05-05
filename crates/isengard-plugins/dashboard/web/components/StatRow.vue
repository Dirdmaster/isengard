<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import type { DeploymentDto } from '~/composables/useDeployments'

/**
 * Home page header strip: hosts up · stacks healthy · pending approvals · deploys in progress.
 *
 * Per `design/pages/home.md` — the 10-second answer to "is anything broken?".
 * Each cell is clickable and routes to the relevant page (where it has somewhere to go).
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
// Anything older drifts toward "stale", anything null is unenrolled / never-reported.
const hostsUp = computed(() => {
  const cutoff = Date.now() - 5 * 60 * 1000
  return hostsStore.hosts.filter((h) => {
    if (!h.last_seen_at) return false
    return new Date(h.last_seen_at).getTime() >= cutoff
  }).length
})

const hostsTotal = computed(() => hostsStore.hosts.length)

// Stacks healthy: anything that doesn't have a recent `update.failed` event
// for one of its services. Heuristic — proper health needs Phase 9-ish data.
const stacksHealthy = computed(() => {
  const failedHostIds = new Set(
    eventsStore.events
      .filter((e) => e.kind === 'update.failed' && e.host_id)
      .slice(0, 50)
      .map((e) => e.host_id as string),
  )
  return stacksStore.items.filter((s) => !failedHostIds.has(s.host_id)).length
})

const stacksTotal = computed(() => stacksStore.items.length)

const deploysInProgress = computed(() => activeDeploys.value.length)

function go(path: string) {
  router.push(path)
}
</script>

<template>
  <div class="grid grid-cols-2 md:grid-cols-4 gap-px bg-iso-border-subtle border-y border-iso-border-subtle shrink-0">
    <button
      type="button"
      class="bg-iso-bg-base hover:bg-iso-bg-elevated transition-colors px-4 py-3 text-left flex flex-col gap-0.5"
      @click="go('/hosts')"
    >
      <div class="flex items-baseline gap-1.5">
        <span class="font-mono text-xl text-iso-text-primary tabular-nums">{{ hostsUp }}</span>
        <span class="text-xs text-iso-text-faint tabular-nums">/ {{ hostsTotal }}</span>
      </div>
      <span class="text-[11px] uppercase tracking-wider text-iso-text-muted">hosts up</span>
    </button>

    <button
      type="button"
      class="bg-iso-bg-base hover:bg-iso-bg-elevated transition-colors px-4 py-3 text-left flex flex-col gap-0.5"
      @click="go('/stacks')"
    >
      <div class="flex items-baseline gap-1.5">
        <span class="font-mono text-xl text-iso-text-primary tabular-nums">{{ stacksHealthy }}</span>
        <span class="text-xs text-iso-text-faint tabular-nums">/ {{ stacksTotal }}</span>
      </div>
      <span class="text-[11px] uppercase tracking-wider text-iso-text-muted">stacks healthy</span>
    </button>

    <div
      class="bg-iso-bg-base px-4 py-3 text-left flex flex-col gap-0.5 cursor-not-allowed"
      title="Approvals coming with Phase 9"
    >
      <div class="flex items-baseline gap-1.5">
        <span class="font-mono text-xl text-iso-text-faint">—</span>
      </div>
      <span class="text-[11px] uppercase tracking-wider text-iso-text-muted">pending approvals</span>
    </div>

    <button
      type="button"
      class="bg-iso-bg-base hover:bg-iso-bg-elevated transition-colors px-4 py-3 text-left flex flex-col gap-0.5"
      @click="go('/stacks')"
    >
      <div class="flex items-baseline gap-1.5">
        <span
          class="font-mono text-xl tabular-nums"
          :class="deploysInProgress > 0 ? 'text-iso-info' : 'text-iso-text-primary'"
        >{{ deploysInProgress }}</span>
      </div>
      <span class="text-[11px] uppercase tracking-wider text-iso-text-muted">deploys in progress</span>
    </button>
  </div>
</template>
