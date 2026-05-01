<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  host: Host
}

const props = defineProps<Props>()
const emit = defineEmits<{ close: []; changed: [] }>()

const fleets = useFleetsStore()
if (fleets.fleets.length === 0) await fleets.load()

const actions = useHostActions()
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

function formatTs(ts: string | null | undefined): string {
  if (!ts) return '—'
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
        <button class="text-iso-text-muted hover:text-iso-text-primary" @click="$emit('close')">
          <Icon name="lucide:x" class="w-4 h-4" />
        </button>
      </header>

      <section class="px-5 py-5 space-y-4">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint">Metadata</div>
        <dl class="grid grid-cols-[120px_1fr] gap-y-2 text-sm font-mono">
          <dt class="text-iso-text-muted">os</dt>           <dd>{{ host.os ?? '—' }}</dd>
          <dt class="text-iso-text-muted">arch</dt>         <dd>{{ host.arch ?? '—' }}</dd>
          <dt class="text-iso-text-muted">agent</dt>        <dd>{{ host.agent_version ?? '—' }}</dd>
          <dt class="text-iso-text-muted">docker</dt>       <dd>{{ host.docker_version ?? '—' }}</dd>
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
          <button class="text-xs px-2 py-1 rounded border border-iso-border-subtle hover:border-iso-success" @click="applyFleet">Apply</button>
          <button class="text-xs text-iso-text-muted" @click="editingFleet = false">Cancel</button>
        </div>
        <p v-if="error" class="text-xs text-iso-error">{{ error }}</p>
      </section>

      <section class="px-5 py-5 border-t border-iso-border-subtle space-y-3">
        <div class="text-xs uppercase tracking-wider text-iso-text-faint">Quick actions</div>
        <button class="w-full px-3 py-2 text-sm rounded border border-iso-border-subtle hover:border-iso-success flex items-center gap-2" @click="forceUpdate">
          <Icon name="lucide:zap" class="w-3.5 h-3.5" />
          Force update all stacks on this host
        </button>
        <NuxtLink :to="`/stacks?host_id=${host.id}`" class="block w-full px-3 py-2 text-sm rounded border border-iso-border-subtle hover:border-iso-success flex items-center gap-2">
          <Icon name="lucide:layers" class="w-3.5 h-3.5" />
          View stacks on this host
        </NuxtLink>
        <button class="w-full px-3 py-2 text-sm rounded border border-iso-border-subtle hover:border-iso-error/50 text-iso-error flex items-center gap-2" @click="decommission">
          <Icon name="lucide:trash-2" class="w-3.5 h-3.5" />
          Decommission host
        </button>
      </section>
    </aside>
  </div>
</template>
