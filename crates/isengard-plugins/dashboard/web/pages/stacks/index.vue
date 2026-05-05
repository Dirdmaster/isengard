<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useServicesStore, type Service } from '~/stores/services'
import { useEventsStore } from '~/stores/events'
import { useUiStore } from '~/stores/ui'
import type { DeploymentDto } from '~/composables/useDeployments'
import type { StackRowData } from '~/components/StacksTable.vue'
import AddStackComingSoonModal from '~/components/AddStackComingSoonModal.vue'

const stacksStore   = useStacksStore()
const hostsStore    = useHostsStore()
const servicesStore = useServicesStore()
const eventsStore   = useEventsStore()
const uiStore       = useUiStore()

await Promise.all([
  stacksStore.fetchAll(),
  hostsStore.load(),
  eventsStore.load(200),
])

// Per-stack deployment lookups.
//   key = stack id (string), value = { active: latest active dep | null, lastFinished: ISO | '' }
interface DepInfo { latestActive: DeploymentDto | null; lastFinishedIso: string }
const depByStack = ref<Record<string, DepInfo>>({})

async function loadDeploymentsForStacks(ids: string[]) {
  const api = useApi()
  const next: Record<string, DepInfo> = { ...depByStack.value }
  await Promise.all(ids.map(async (sid) => {
    const numeric = Number(sid)
    if (!Number.isFinite(numeric)) {
      next[sid] = { latestActive: null, lastFinishedIso: '' }
      return
    }
    try {
      const [active, history] = await Promise.all([
        api.get<DeploymentDto[]>('/deployments', { stack_id: numeric, state: 'active' }),
        api.get<DeploymentDto[]>('/deployments', { stack_id: numeric, state: 'history', limit: 1 }),
      ])
      const latestActive = active.length
        ? [...active].sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime())[0]
        : null
      const lastFinishedIso = history.length
        ? (history[0].finished_at || history[0].updated_at || '')
        : ''
      next[sid] = { latestActive, lastFinishedIso }
    } catch {
      next[sid] = { latestActive: null, lastFinishedIso: '' }
    }
  }))
  depByStack.value = next
}

// Per-stack services. Backend currently returns [] for /services until 5e
// lands the service table; the lookup is harmless and future-proof.
const servicesByStack = ref<Record<string, Service[]>>({})

async function loadServicesForStacks(ids: string[]) {
  const next: Record<string, Service[]> = { ...servicesByStack.value }
  await Promise.all(ids.map(async (sid) => {
    try {
      await servicesStore.fetchByStack(sid)
      next[sid] = servicesStore.byStack(sid)
    } catch {
      next[sid] = []
    }
  }))
  servicesByStack.value = next
}

// Trigger lookups whenever the stacks list changes.
watchEffect(() => {
  const ids = stacksStore.items.map((s) => s.id)
  if (ids.length) {
    loadDeploymentsForStacks(ids)
    loadServicesForStacks(ids)
  }
})

const rows = computed<StackRowData[]>(() => {
  const fleet = uiStore.activeFleet
  return stacksStore.items
    .map((stack): StackRowData | null => {
      const host = hostsStore.hosts.find((h) => h.id === stack.host_id)
      if (!host) return null
      if (fleet !== 'all' && host.fleet !== fleet) return null

      const services = servicesByStack.value[stack.id] ?? []
      const dep = depByStack.value[stack.id] ?? { latestActive: null, lastFinishedIso: '' }

      // Image: longest image string is usually the most-specific (registry/path:tag)
      // and a reasonable proxy for "primary" service when no other signal exists.
      let primaryImage: string | null = null
      let extraImageCount = 0
      if (services.length) {
        const images = services.map((s) => s.image).filter((img): img is string => !!img)
        if (images.length) {
          primaryImage = images.reduce((longest, img) => img.length > longest.length ? img : longest)
          extraImageCount = Math.max(0, images.length - 1)
        }
      }

      // Health: latest active deployment's state takes precedence; otherwise
      // fall back to "unknown" until services are persisted (5e). When all
      // reported services are running, surface "healthy" early.
      let health: StackRowData['health'] = 'unknown'
      const active = dep.latestActive
      if (active) {
        switch (active.state) {
          case 'failed': health = 'failed'; break
          case 'aborted': health = 'aborted'; break
          case 'pending':
          case 'running':
          case 'switching':
          case 'draining': health = 'updating'; break
          case 'done': health = 'healthy'; break
          default: health = 'unknown'
        }
      } else if (services.length && services.every((s) => s.state === 'running')) {
        health = 'healthy'
      }

      return {
        stack,
        hostHostname: host.hostname,
        fleet: host.fleet,
        serviceCount: services.length,
        primaryImage,
        extraImageCount,
        health,
        lastDeployIso: dep.lastFinishedIso,
      }
    })
    .filter((r): r is StackRowData => r !== null)
})

const subtitle = computed(() => {
  const stackCount = rows.value.length
  const serviceCount = rows.value.reduce((sum, r) => sum + r.serviceCount, 0)
  const hostSet = new Set(rows.value.map((r) => r.stack.host_id))
  const hostCount = hostSet.size
  const stacksLabel = `${stackCount} ${stackCount === 1 ? 'stack' : 'stacks'}`
  const servicesLabel = `${serviceCount} ${serviceCount === 1 ? 'service' : 'services'}`
  if (hostCount === 0) return `${stacksLabel} · ${servicesLabel}`
  return `${stacksLabel} · ${servicesLabel} across ${hostCount} ${hostCount === 1 ? 'host' : 'hosts'}`
})

const addStackOpen = ref(false)
</script>

<template>
  <AppShell>
    <PageHeader title="Stacks" :subtitle="subtitle">
      <template #actions>
        <button
          class="px-3 py-1.5 rounded-iso-md bg-iso-success text-iso-bg-base text-xs font-medium hover:opacity-90"
          @click="addStackOpen = true"
        >
          + Add stack
        </button>
      </template>
    </PageHeader>

    <div class="flex-1 flex flex-col min-h-0 overflow-y-auto">
      <TableSkeleton v-if="!stacksStore.loaded" :rows="6" :columns="[240, 110, 90, 240, 140, 110]" />
      <StacksTable v-else :rows="rows" class="flex-1 flex flex-col min-h-0" />
    </div>

    <AddStackComingSoonModal v-if="addStackOpen" @close="addStackOpen = false" />
  </AppShell>
</template>
