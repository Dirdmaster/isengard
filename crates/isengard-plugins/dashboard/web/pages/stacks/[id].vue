<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useServicesStore } from '~/stores/services'
import { useEventsStore } from '~/stores/events'
import ServiceExposeModal from '~/components/ServiceExposeModal.vue'
import DeploymentInProgressPanel from '~/components/DeploymentInProgressPanel.vue'
import DeploymentAbortedPanel from '~/components/DeploymentAbortedPanel.vue'
import DeploymentGroupPanel from '~/components/stacks/DeploymentGroupPanel.vue'
import StackTabs from '~/components/stacks/StackTabs.vue'
import StackOverviewTab from '~/components/stacks/StackOverviewTab.vue'
import StackHistoryTab from '~/components/stacks/StackHistoryTab.vue'
import StackComposeTab from '~/components/stacks/StackComposeTab.vue'
import StackRoutingTab from '~/components/stacks/StackRoutingTab.vue'
import StackSettingsTab from '~/components/stacks/StackSettingsTab.vue'
import { useDeployments } from '~/composables/useDeployments'

const route = useRoute()
const stackId = computed(() => route.params.id as string)

const stacksStore   = useStacksStore()
const hostsStore    = useHostsStore()
const servicesStore = useServicesStore()
const eventsStore   = useEventsStore()

await stacksStore.fetchOne(stackId.value)
const stack = computed(() => stacksStore.byId(stackId.value))

watchEffect(async () => {
  if (stack.value) {
    await Promise.all([
      hostsStore.load(),
      servicesStore.fetchByStack(stack.value.id),
      eventsStore.load(50),
    ])
  }
})

const host = computed(() =>
  stack.value
    ? hostsStore.hosts.find((h) => h.id === stack.value!.host_id)
    : undefined
)
const services = computed(() => stack.value ? servicesStore.byStack(stack.value.id) : [])
const recentEvents = computed(() => {
  if (!stack.value) return []
  return eventsStore.events
    .filter((e) => e.host_id === stack.value!.host_id)
    .slice(0, 20)
})

// Live deployments for this stack. Most recently created `active` deployment
// is shown above the services section (Plan B Phase 10 Task 6).
// When no active deployment exists but a recent terminal one (aborted/failed)
// surfaced in the last 5 minutes, the aborted panel takes its slot until the
// user dismisses it (Task 10).
const { active: activeDeployments, history: deploymentHistory } = useDeployments(stackId)
const visibleDeployment = computed(() => {
  if (!activeDeployments.value.length) return null
  return [...activeDeployments.value].sort((a, b) => {
    return new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
  })[0]
})

const dismissedAborted = ref(new Set<string>())
const visibleAborted = computed(() => {
  if (visibleDeployment.value) return null
  const now = Date.now()
  const fiveMinutes = 5 * 60 * 1000
  // Sort newest first by finished_at|updated_at then take first non-done,
  // non-dismissed, recent row.
  const rows = [...deploymentHistory.value].sort((a, b) => {
    const ta = new Date(a.finished_at || a.updated_at).getTime()
    const tb = new Date(b.finished_at || b.updated_at).getTime()
    return tb - ta
  })
  for (const d of rows) {
    if (dismissedAborted.value.has(d.id)) continue
    if (d.state === 'done') continue
    const ts = new Date(d.finished_at || d.updated_at).getTime()
    if (now - ts > fiveMinutes) continue
    return d
  }
  return null
})

function dismissAborted(id: string) {
  dismissedAborted.value.add(id)
  // Trigger reactivity on the Set wrapper.
  dismissedAborted.value = new Set(dismissedAborted.value)
}

async function forceUpdate() {
  try {
    const api = useApi()
    await api.post(`/stacks/${stackId.value}/actions/force-update`, {})
    useToast().success(`Force update queued for stack`)
  } catch (e) {
    useToast().error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function abortDeploy(id: string) {
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: 'Abort deployment?',
    description: 'The current deploy will be stopped. Containers may be left in a partial state until the next update.',
    confirmText: 'Abort',
    danger: true,
  })
  if (!ok) return
  try {
    const api = useApi()
    await api.post(`/deployments/${id}/abort`, {})
    useToast().success('Abort requested')
  } catch (e) {
    useToast().error(`Abort failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

const exposeModalOpen = ref(false)
const exposeModalHostId = ref('')
const exposeModalServiceName = ref('')
const exposeModalPort = ref(0)
function openExposeFor(hostId: string, serviceName: string, port: number) {
  exposeModalHostId.value = hostId
  exposeModalServiceName.value = serviceName
  exposeModalPort.value = port
  exposeModalOpen.value = true
}

const stackTabs = [
  { key: 'overview', label: 'Overview' },
  { key: 'compose', label: 'Compose' },
  { key: 'history', label: 'History' },
  { key: 'routing', label: 'Routing' },
  { key: 'settings', label: 'Settings' },
]
</script>

<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />
    <div v-if="stack && host" class="flex-1 overflow-y-auto">
      <StackHeader
        :stack="stack"
        :host-hostname="host.hostname"
        :fleet="''"
        :active-deployment="visibleDeployment"
        @force-update="forceUpdate"
        @abort-deploy="abortDeploy"
      />

      <!-- Phase 10c (T5 refs #50): multi-host group panel sits above the
           single-deployment panel. Single-host deploys never produce a
           group row, so this slot stays empty for the homelab default. -->
      <div class="px-6 pt-6">
        <DeploymentGroupPanel :stack-id="stackId" />
      </div>
      <div class="px-6 pt-6" v-if="visibleDeployment">
        <DeploymentInProgressPanel :deployment="visibleDeployment" />
      </div>
      <div class="px-6 pt-6" v-else-if="visibleAborted">
        <DeploymentAbortedPanel
          :deployment="visibleAborted"
          @dismiss="dismissAborted(visibleAborted.id)"
        />
      </div>

      <StackTabs :tabs="stackTabs" default-tab="overview" v-slot="{ activeTab }">
        <StackOverviewTab
          v-if="activeTab === 'overview'"
          :stack="stack"
          :services="services"
          :recent-events="recentEvents"
          :fleet="''"
          @expose="openExposeFor"
        />
        <StackComposeTab v-else-if="activeTab === 'compose'" :stack-id="stackId" />
        <StackHistoryTab v-else-if="activeTab === 'history'" :stack-id="stackId" />
        <StackRoutingTab
          v-else-if="activeTab === 'routing'"
          :stack-id="stackId"
          :host-id="stack.host_id"
        />
        <StackSettingsTab
          v-else-if="activeTab === 'settings'"
          :stack-id="stackId"
          :host-id="stack.host_id"
          :stack-name="stack.name"
          :fleet="''"
        />
      </StackTabs>
    </div>

    <div v-else class="p-6 text-iso-text-muted">
      Stack not found.
    </div>

    <ServiceExposeModal
      v-if="exposeModalOpen"
      v-model:open="exposeModalOpen"
      :host-id="exposeModalHostId"
      :service-name="exposeModalServiceName"
      :container-port="exposeModalPort"
    />
  </div>
</template>
