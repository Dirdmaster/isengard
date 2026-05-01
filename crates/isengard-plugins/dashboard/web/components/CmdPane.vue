<template>
  <Teleport to="body">
    <div v-if="ui.cmdPaneOpen" class="fixed inset-0 z-50 flex items-start justify-center bg-black/60 backdrop-blur-sm pt-[180px]" @click.self="ui.closeCmdPane()">
      <div class="w-[640px] max-w-full bg-iso-bg-overlay border border-iso-border-strong rounded-iso-lg shadow-2xl overflow-hidden flex flex-col" @click.stop style="max-height: 70vh">
        <CmdInput v-model="query" @keydown="onKey" />

        <div class="flex-1 overflow-y-auto py-1.5">
          <template v-if="results.hosts.length > 0">
            <CmdSection label="Hosts" />
            <CmdResultRow
              v-for="(h, i) in results.hosts"
              :key="`h-${h.id}`"
              icon="lucide:server"
              :label="h.hostname"
              :meta="`${h.fleet} · ${h.fingerprint.slice(0, 12)}`"
              :highlighted="selectedIdx === i"
              @select="navigateToHost(h)"
            />
          </template>

          <template v-if="results.events.length > 0">
            <CmdSection label="Events" />
            <CmdResultRow
              v-for="(e, i) in results.events"
              :key="`e-${e.id}`"
              icon="lucide:activity"
              :label="e.summary"
              :meta="e.kind"
              :highlighted="selectedIdx === results.hosts.length + i"
              @select="selectEvent(e)"
            />
          </template>

          <template v-if="results.actions.length > 0">
            <CmdSection label="Actions" />
            <CmdResultRow
              v-for="(a, i) in results.actions"
              :key="`a-${a.label}`"
              :icon="a.icon"
              :label="a.label"
              :meta="a.meta"
              :highlighted="selectedIdx === results.hosts.length + results.events.length + i"
              @select="a.run()"
            />
          </template>

          <div v-if="totalResults === 0 && query.length > 0" class="px-5 py-6 text-center text-iso-text-faint text-iso-sm">
            No matches. Try a host, event, or action.
          </div>
          <div v-if="totalResults === 0 && query.length === 0" class="px-5 py-3 text-iso-sm text-iso-text-muted">
            <p>Type to search hosts, events, or run actions.</p>
            <p class="text-iso-xs mt-2 text-iso-text-faint">: for actions · $ for shell · ? for help</p>
          </div>
        </div>

        <div class="h-9 px-4 border-t border-iso-border-subtle flex items-center gap-3.5 text-iso-xs font-mono text-iso-text-faint">
          <span>↑↓ navigate</span>
          <span>⏎ select</span>
          <div class="flex-1"></div>
          <span>⌘. dock · esc close</span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import Fuse from 'fuse.js'

const ui = useUiStore()
const router = useRouter()
const hostsStore = useHostsStore()
const eventsStore = useEventsStore()

const query = ref('')
const selectedIdx = ref(0)

watch(() => ui.cmdPaneOpen, (open) => {
  if (open) {
    query.value = ''
    selectedIdx.value = 0
  }
})

const hostFuse = computed(() => new Fuse(hostsStore.hosts, { keys: ['hostname', 'fingerprint', 'fleet'] }))
const eventFuse = computed(() => new Fuse(eventsStore.events, { keys: ['summary', 'kind', 'container_name'] }))

const defaultActions = computed(() => [
  { icon: 'lucide:zap', label: 'Force update cycle on all hosts', meta: 'runs now', run: () => alert('TODO 5d: wire force-update RPC') },
  { icon: 'lucide:terminal', label: 'Open shell on a container', meta: 'pick container next', run: () => alert('TODO 5e: cmd pane terminal mode') },
])

const results = computed(() => {
  if (query.value.length === 0) {
    return { hosts: hostsStore.hosts.slice(0, 5), events: [] as any[], actions: defaultActions.value }
  }
  return {
    hosts: hostFuse.value.search(query.value).slice(0, 5).map(r => r.item),
    events: eventFuse.value.search(query.value).slice(0, 5).map(r => r.item),
    actions: defaultActions.value.filter(a =>
      a.label.toLowerCase().includes(query.value.toLowerCase())
    ),
  }
})

const totalResults = computed(() => results.value.hosts.length + results.value.events.length + results.value.actions.length)

function onKey(e: KeyboardEvent) {
  if (e.key === 'ArrowDown') { e.preventDefault(); selectedIdx.value = Math.min(selectedIdx.value + 1, totalResults.value - 1) }
  else if (e.key === 'ArrowUp') { e.preventDefault(); selectedIdx.value = Math.max(0, selectedIdx.value - 1) }
  else if (e.key === 'Enter') { e.preventDefault(); selectActive() }
}

function selectActive() {
  const hostsLen = results.value.hosts.length
  const eventsLen = results.value.events.length
  const i = selectedIdx.value
  if (i < hostsLen) navigateToHost(results.value.hosts[i])
  else if (i < hostsLen + eventsLen) selectEvent(results.value.events[i - hostsLen])
  else {
    const actionIdx = i - hostsLen - eventsLen
    results.value.actions[actionIdx]?.run()
  }
}

function navigateToHost(h: any) {
  ui.closeCmdPane()
  router.push(`/hosts/${h.id}`)
}

function selectEvent(e: any) {
  ui.selectEvent(e.id)
  ui.closeCmdPane()
}
</script>
