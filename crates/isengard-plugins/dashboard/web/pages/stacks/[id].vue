<script setup lang="ts">
import { useStacksStore } from '~/stores/stacks'
import { useHostsStore } from '~/stores/hosts'
import { useServicesStore } from '~/stores/services'
import { useEventsStore } from '~/stores/events'
import ServiceExposeModal from '~/components/ServiceExposeModal.vue'

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

async function forceUpdate() {
  try {
    const api = useApi()
    await api.post(`/stacks/${stackId.value}/actions/force-update`, {})
    useToast().success(`Force update queued for stack`)
  } catch (e) {
    useToast().error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
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
</script>

<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />
    <div v-if="stack && host" class="flex-1 overflow-y-auto">
      <StackHeader
        :stack="stack"
        :host-hostname="host.hostname"
        :fleet="host.fleet"
        @force-update="forceUpdate"
      />

      <div class="grid grid-cols-2 gap-6 p-6">
        <section>
          <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Services</h2>
          <div class="flex flex-wrap gap-2 items-center">
            <template v-for="svc in services" :key="svc.name">
              <div class="flex items-center gap-1">
                <ServiceChip :name="svc.name" :state="svc.state" />
                <button
                  class="text-[10px] text-iso-text-muted hover:text-iso-info underline px-1"
                  @click="openExposeFor(svc.host_id, svc.name, 0)"
                >
                  Expose
                </button>
              </div>
            </template>
            <span v-if="services.length === 0" class="text-sm text-iso-text-faint">
              No services reported (waiting for next heartbeat).
            </span>
          </div>
        </section>

        <section>
          <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Recent events</h2>
          <div class="space-y-1">
            <EventRow
              v-for="e in recentEvents"
              :key="e.id"
              :event="e"
              :selected="false"
            />
            <span v-if="recentEvents.length === 0" class="text-sm text-iso-text-faint">
              No recent events for this stack's host.
            </span>
          </div>
        </section>
      </div>
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
