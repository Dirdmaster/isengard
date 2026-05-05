<script setup lang="ts">
import type { Host } from '~/stores/hosts'
import { useEnrollment } from '~/composables/useEnrollment'

interface Props {
  host: Host
}

const props = defineProps<Props>()
const emit = defineEmits<{ close: []; changed: [] }>()

const fleets = useFleetsStore()
if (fleets.fleets.length === 0) await fleets.load()

const actions = useHostActions()
const enrollment = useEnrollment()
const toast = useToast()
const editingFleet = ref(false)
const newFleet = ref(props.host.fleet)
const error = ref('')

async function applyFleet() {
  error.value = ''
  try {
    await actions.setFleet(props.host.id, newFleet.value)
    editingFleet.value = false
    toast.success(`Fleet updated to ${newFleet.value}`)
    emit('changed')
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    error.value = msg
    toast.error(`Set fleet failed: ${msg}`)
  }
}

async function forceUpdate() {
  try {
    await actions.forceUpdate(props.host.id)
    toast.success(`Force update queued for ${props.host.hostname}`)
  } catch (e) {
    toast.error(`Force update failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function decommission() {
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: `Decommission ${props.host.hostname}?`,
    description: 'This revokes its enrollment token and removes it from inventory. The agent on the host will no longer report. This cannot be undone.',
    confirmText: 'Decommission',
    danger: true,
  })
  if (!ok) return

  try {
    await actions.decommission(props.host.id)
    toast.success(`${props.host.hostname} decommissioned`)
    emit('close')
    emit('changed')
  } catch (e) {
    toast.error(`Decommission failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

async function revokeCert() {
  const { confirm } = useConfirm()
  const ok = await confirm({
    title: `Revoke cert for ${props.host.hostname}?`,
    description:
      'The active leaf cert is invalidated immediately: the next gRPC call from this agent will be rejected. The host stays in inventory and can be re-enrolled with a fresh token.',
    confirmText: 'Revoke cert',
    danger: true,
  })
  if (!ok) return

  try {
    await enrollment.revokeHostCert(props.host.id)
    toast.success(`Certificate for ${props.host.hostname} revoked`)
    emit('changed')
  } catch (e) {
    toast.error(`Revoke cert failed: ${e instanceof Error ? e.message : String(e)}`)
  }
}

function formatTs(ts: string | null | undefined): string {
  if (!ts) return 'unknown'
  return new Date(ts).toLocaleString()
}
</script>

<template>
  <div class="fixed inset-0 z-40 flex justify-end" @click.self="$emit('close')">
    <div class="absolute inset-0 bg-black/30"></div>
    <aside class="relative z-10 w-[420px] h-full bg-iso-bg-base border-l border-iso-border-subtle overflow-y-auto">
      <header class="flex items-center justify-between px-5 py-4 border-b border-iso-border-subtle">
        <div class="flex items-center gap-3">
          <span class="w-2.5 h-2.5 rounded-full bg-iso-success"></span>
          <h2 class="font-mono text-base">{{ host.hostname }}</h2>
          <span class="text-xs text-iso-text-muted">· {{ host.fleet }}</span>
        </div>
        <Button variant="ghost" size="icon" class="text-iso-text-muted hover:text-iso-text-primary" @click="$emit('close')">
          <Icon name="lucide:x" class="w-4 h-4" />
        </Button>
      </header>

      <section class="px-5 py-5 space-y-4">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint">Metadata</div>
        <dl class="grid grid-cols-[120px_1fr] gap-y-2 text-sm font-mono">
          <template v-if="host.os"><dt class="text-iso-text-muted">os</dt><dd>{{ host.os }}</dd></template>
          <template v-if="host.arch"><dt class="text-iso-text-muted">arch</dt><dd>{{ host.arch }}</dd></template>
          <template v-if="host.agent_version"><dt class="text-iso-text-muted">agent</dt><dd>{{ host.agent_version }}</dd></template>
          <template v-if="host.docker_version"><dt class="text-iso-text-muted">docker</dt><dd>{{ host.docker_version }}</dd></template>
          <dt class="text-iso-text-muted">enrolled</dt>     <dd class="text-iso-text-secondary">{{ formatTs(host.enrolled_at) }}</dd>
          <dt class="text-iso-text-muted">last seen</dt>    <dd class="text-iso-text-secondary">{{ formatTs(host.last_seen_at) }}</dd>
          <dt class="text-iso-text-muted">fingerprint</dt>  <dd class="text-iso-text-faint truncate">{{ host.fingerprint }}</dd>
        </dl>
      </section>

      <section class="px-5 py-5 border-t border-iso-border-subtle space-y-3">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint">Fleet</div>
        <div v-if="!editingFleet" class="flex items-center gap-3">
          <span class="font-mono text-sm">{{ host.fleet }}</span>
          <button class="text-xs text-iso-text-muted hover:text-iso-text-primary underline" @click="editingFleet = true">Change</button>
        </div>
        <div v-else class="flex items-center gap-2">
          <select v-model="newFleet" class="bg-iso-bg-elevated border border-iso-border-subtle rounded px-2 py-1 text-sm font-mono">
            <option v-for="f in fleets.fleets" :key="f.name" :value="f.name">{{ f.name }}</option>
          </select>
          <Button size="sm" variant="outline" @click="applyFleet">Apply</Button>
          <Button size="sm" variant="ghost" @click="editingFleet = false">Cancel</Button>
        </div>
        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </section>

      <section class="px-5 py-5 border-t border-iso-border-subtle space-y-2">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Quick actions</div>
        <Button
          variant="outline"
          class="w-full justify-start border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
          @click="forceUpdate"
        >
          <Icon name="lucide:zap" class="w-3.5 h-3.5 mr-2" />
          Force update all stacks on this host
        </Button>
        <Button
          variant="outline"
          as-child
          class="w-full justify-start border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        >
          <NuxtLink :to="`/stacks?host_id=${host.id}`">
            <Icon name="lucide:layers" class="w-3.5 h-3.5 mr-2" />
            View stacks on this host
          </NuxtLink>
        </Button>
        <Button
          variant="outline"
          class="w-full justify-start border-iso-error/40 text-iso-error hover:bg-iso-error/10 hover:border-iso-error"
          @click="revokeCert"
        >
          <Icon name="lucide:shield-off" class="w-3.5 h-3.5 mr-2" />
          Revoke cert
        </Button>
        <Button
          variant="outline"
          class="w-full justify-start border-iso-error/40 text-iso-error hover:bg-iso-error/10 hover:border-iso-error"
          @click="decommission"
        >
          <Icon name="lucide:trash-2" class="w-3.5 h-3.5 mr-2" />
          Decommission host
        </Button>
      </section>
    </aside>
  </div>
</template>
