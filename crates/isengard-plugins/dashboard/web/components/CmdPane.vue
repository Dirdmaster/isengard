<template>
  <Teleport to="body">
    <div
      v-if="ui.cmdPaneOpen"
      class="fixed inset-0 z-50 flex items-start justify-center pt-[140px] cmd-backdrop"
      @click.self="ui.closeCmdPane()"
    >
      <div
        v-if="ui.cmdPaneMode === 'navigator'"
        class="w-[640px] max-w-full h-[520px] bg-iso-bg-overlay border border-iso-border-strong rounded-[10px] overflow-hidden flex flex-col cmd-panel"
        @click.stop
      >
        <CmdInput v-model="query" @keydown="onKey" />

        <div ref="resultsRef" class="flex-1 overflow-y-auto py-1.5">
          <template v-if="results.hosts.length > 0">
            <CmdSection label="Hosts" />
            <CmdResultRow
              v-for="(h, i) in results.hosts"
              :key="`h-${h.id}`"
              icon="lucide:server"
              :label="h.hostname"
              :meta="hostMeta(h)"
              :highlighted="selectedIdx === i"
              :hint="selectedIdx === i ? 'open' : undefined"
              hint-icon="lucide:corner-down-left"
              @select="navigateToHost(h)"
              @hover="selectedIdx = i"
            />
          </template>

          <template v-if="results.containers.length > 0">
            <CmdSection label="Containers" />
            <CmdResultRow
              v-for="(c, i) in results.containers"
              :key="`c-${c.id}`"
              icon="lucide:box"
              :label="c.name"
              :meta="containerMeta(c)"
              :highlighted="selectedIdx === results.hosts.length + i"
              :hint="selectedIdx === results.hosts.length + i ? 'open' : undefined"
              hint-icon="lucide:corner-down-left"
              @select="navigateToContainer(c)"
              @hover="selectedIdx = results.hosts.length + i"
            />
          </template>

          <template v-if="results.actions.length > 0">
            <CmdSection label="Actions" />
            <CmdResultRow
              v-for="(a, i) in results.actions"
              :key="`a-${a.key}`"
              :icon="a.icon"
              :icon-class="a.iconClass"
              :label="a.label"
              :meta="a.meta"
              :highlighted="selectedIdx === results.hosts.length + results.containers.length + i"
              :hint="selectedIdx === results.hosts.length + results.containers.length + i ? (a.disabled ? 'soon' : 'run') : undefined"
              hint-icon="lucide:corner-down-left"
              @select="runAction(a)"
              @hover="selectedIdx = results.hosts.length + results.containers.length + i"
            />
          </template>

          <div
            v-if="totalResults === 0 && query.length > 0"
            class="px-5 py-6 text-center text-iso-text-faint text-iso-sm"
          >
            No matches. Try a host, container, or action.
          </div>
        </div>

        <CmdFooter />
      </div>

      <div
        v-else-if="ui.cmdPaneMode === 'terminal' && ui.cmdPaneTerminal"
        class="absolute bottom-0 left-0 right-0 h-[400px] bg-iso-bg-overlay border-t border-iso-border-strong shadow-2xl"
        @click.stop
      >
        <ClientOnly>
          <CmdTerminal
            :service-id="ui.cmdPaneTerminal.serviceId"
            :service-name="ui.cmdPaneTerminal.serviceName"
            :host-hostname="ui.cmdPaneTerminal.hostHostname"
            :fleet="ui.cmdPaneTerminal.fleet"
            :stack-name="ui.cmdPaneTerminal.stackName"
            @toggle-position="ui.toggleCmdPanePosition()"
            @close="ui.closeCmdPane()"
          />
        </ClientOnly>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import Fuse from 'fuse.js'
import type { Host } from '~/stores/hosts'
import type { Service } from '~/stores/services'

const ui = useUiStore()
const router = useRouter()
const toast = useToast()
const hostActions = useHostActions()
const hostsStore = useHostsStore()
const stacksStore = useStacksStore()
const servicesStore = useServicesStore()

const query = ref('')
const selectedIdx = ref(0)
const resultsRef = ref<HTMLElement>()

watch(() => ui.cmdPaneOpen, async (open) => {
  if (open) {
    query.value = ''
    selectedIdx.value = 0
    if (!stacksStore.loaded) await stacksStore.fetchAll()
  }
})

watch(query, () => { selectedIdx.value = 0 })

// ─── Search indexes ──────────────────────────────────────────────────────
const hostFuse = computed(() => new Fuse(hostsStore.hosts, { keys: ['hostname', 'fleet', 'fingerprint'], threshold: 0.4 }))
const serviceFuse = computed(() => new Fuse(servicesStore.items, { keys: ['name', 'image'], threshold: 0.4 }))

// ─── Action catalogue ────────────────────────────────────────────────────
interface CmdAction {
  key: string
  icon: string
  iconClass?: string
  label: string
  meta: string
  disabled?: boolean
  run: () => void | Promise<void>
}

const baseActions = computed<CmdAction[]>(() => {
  const actions: CmdAction[] = []
  const q = query.value.trim()

  // Filter events to "<query>"
  if (q.length > 0) {
    actions.push({
      key: 'filter-events',
      icon: 'lucide:filter',
      iconClass: 'text-iso-info',
      label: `Filter events to "${q}"`,
      meta: 'opens /events',
      run: () => {
        ui.closeCmdPane()
        router.push({ path: '/events', query: { q } })
      },
    })
  }

  // Open shell — first matching service for the query, if any
  const shellTarget = q.length > 0
    ? servicesStore.items.find(s => s.name.toLowerCase().includes(q.toLowerCase()))
    : servicesStore.items[0]
  if (shellTarget) {
    const host = hostsStore.hosts.find(h => h.id === shellTarget.host_id)
    actions.push({
      key: `shell-${shellTarget.id}`,
      icon: 'lucide:terminal-square',
      iconClass: 'text-iso-text-secondary',
      label: `Open shell on ${shellTarget.name}${host ? ` @ ${host.hostname}` : ''}`,
      meta: '$ docker exec -it',
      disabled: true, // no backend shell endpoint yet
      run: () => toast.info('Shell endpoint not available yet'),
    })
  }

  // Force update cycle — first matching host, if any
  const fleetTarget = q.length > 0
    ? hostsStore.hosts.find(h => h.hostname.toLowerCase().includes(q.toLowerCase()) || h.fleet.toLowerCase().includes(q.toLowerCase()))
    : hostsStore.hosts[0]
  if (fleetTarget) {
    actions.push({
      key: `force-update-${fleetTarget.id}`,
      icon: 'lucide:zap',
      iconClass: 'text-iso-warn',
      label: `Force update cycle on ${fleetTarget.hostname}`,
      meta: 'runs now · confirm before',
      run: async () => {
        ui.closeCmdPane()
        try {
          await hostActions.forceUpdate(fleetTarget.id)
          toast.success(`Force update queued on ${fleetTarget.hostname}`)
        } catch (e) {
          toast.error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
        }
      },
    })
  }

  actions.push({
    key: 'mint-token',
    icon: 'lucide:key',
    iconClass: 'text-iso-info',
    label: 'Mint enrollment token',
    meta: 'opens /settings',
    run: () => {
      ui.closeCmdPane()
      router.push({ path: '/settings', query: { tab: 'enrollment' } })
    },
  })

  actions.push({
    key: 'open-help',
    icon: 'lucide:circle-help',
    iconClass: 'text-iso-text-muted',
    label: 'Open dashboard help',
    meta: 'shortcut: ?',
    run: () => {
      ui.closeCmdPane()
      ui.helpOpen = true
    },
  })

  return actions
})

// ─── Result groups ───────────────────────────────────────────────────────
const MAX_PER_GROUP = 5

const results = computed(() => {
  const q = query.value.trim()
  let hosts: Host[]
  let containers: Service[]
  let actions: CmdAction[]

  if (q.length === 0) {
    hosts = hostsStore.hosts.slice(0, MAX_PER_GROUP)
    containers = servicesStore.items.slice(0, MAX_PER_GROUP)
    actions = baseActions.value
  } else {
    hosts = hostFuse.value.search(q).slice(0, MAX_PER_GROUP).map(r => r.item)
    containers = serviceFuse.value.search(q).slice(0, MAX_PER_GROUP).map(r => r.item)
    const lq = q.toLowerCase()
    actions = baseActions.value.filter(a => a.label.toLowerCase().includes(lq) || a.meta.toLowerCase().includes(lq))
  }

  return { hosts, containers, actions }
})

const totalResults = computed(() =>
  results.value.hosts.length + results.value.containers.length + results.value.actions.length,
)

// ─── Meta formatters ─────────────────────────────────────────────────────
function lastSeenRelative(ts: string | null): string {
  if (!ts) return 'never seen'
  const ms = Date.now() - new Date(ts).getTime()
  const secs = Math.floor(ms / 1000)
  if (secs < 10) return 'just now'
  if (secs < 60) return `${secs}s ago`
  const mins = Math.floor(secs / 60)
  if (mins < 60) return `${mins}m ago`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h ago`
  return `${Math.floor(hrs / 24)}d ago`
}

function hostMeta(h: Host): string {
  const stackCount = stacksStore.items.filter(s => s.host_id === h.id).length
  const containerCount = servicesStore.items.filter(s => s.host_id === h.id).length
  const left = containerCount > 0
    ? `${containerCount} container${containerCount === 1 ? '' : 's'}`
    : `${stackCount} stack${stackCount === 1 ? '' : 's'}`
  return `${left} · last seen ${lastSeenRelative(h.last_seen_at)}`
}

function containerMeta(c: Service): string {
  const host = hostsStore.hosts.find(h => h.id === c.host_id)
  const parts: string[] = []
  if (host) parts.push(`on ${host.hostname}`)
  if (c.image) parts.push(c.image)
  return parts.join(' · ')
}

// ─── Keyboard nav ────────────────────────────────────────────────────────
function onKey(e: KeyboardEvent) {
  if (e.key === 'ArrowDown') {
    e.preventDefault()
    selectedIdx.value = Math.min(selectedIdx.value + 1, Math.max(0, totalResults.value - 1))
    scrollSelectedIntoView()
  } else if (e.key === 'ArrowUp') {
    e.preventDefault()
    selectedIdx.value = Math.max(0, selectedIdx.value - 1)
    scrollSelectedIntoView()
  } else if (e.key === 'Enter') {
    e.preventDefault()
    selectActive()
  }
}

function scrollSelectedIntoView() {
  nextTick(() => {
    const el = resultsRef.value?.querySelectorAll('button')[selectedIdx.value] as HTMLElement | undefined
    el?.scrollIntoView({ block: 'nearest' })
  })
}

function selectActive() {
  const hostsLen = results.value.hosts.length
  const contLen = results.value.containers.length
  const i = selectedIdx.value
  if (i < hostsLen) {
    navigateToHost(results.value.hosts[i])
  } else if (i < hostsLen + contLen) {
    navigateToContainer(results.value.containers[i - hostsLen])
  } else {
    const a = results.value.actions[i - hostsLen - contLen]
    if (a) runAction(a)
  }
}

function navigateToHost(h: Host) {
  ui.closeCmdPane()
  router.push({ path: '/stacks', query: { host_id: h.id } })
}

function navigateToContainer(c: Service) {
  ui.closeCmdPane()
  if (c.stack_id) {
    router.push(`/stacks/${c.stack_id}`)
  } else {
    router.push({ path: '/stacks', query: { host_id: c.host_id } })
  }
}

function runAction(a: CmdAction) {
  if (a.disabled) {
    toast.info(`${a.label.replace(/^Open shell on /, 'Shell on ')} is not available yet`)
    return
  }
  void a.run()
}
</script>

<style scoped>
.cmd-backdrop {
  background-color: rgba(0, 0, 0, 0.62);
}
.cmd-panel {
  box-shadow: 0 16px 40px rgba(0, 0, 0, 0.75);
}
</style>
