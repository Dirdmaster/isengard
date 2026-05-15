<script setup lang="ts">
import { computed } from 'vue'
import type { Host } from '~/stores/hosts'
import { useEnrollment } from '~/composables/useEnrollment'

interface Props {
  host: Host
}

const props = defineProps<Props>()
const emit = defineEmits<{ close: []; changed: [] }>()

const actions = useHostActions()
const enrollment = useEnrollment()
const toast = useToast()

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

function lastSeenRelative(ts: string | null | undefined): { text: string; cls: string } {
  if (!ts) return { text: 'never', cls: 'text-iso-text-faint' }
  const ms = Date.now() - new Date(ts).getTime()
  const secs = Math.floor(ms / 1000)
  if (secs < 10) return { text: 'just now', cls: 'text-iso-success' }
  if (secs < 60) return { text: `${secs}s ago`, cls: 'text-iso-success' }
  const mins = Math.floor(secs / 60)
  if (mins < 5) return { text: `${mins}m ago`, cls: 'text-iso-success' }
  if (mins < 60) return { text: `${mins}m ago`, cls: 'text-iso-text-secondary' }
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return { text: `${hrs}h ago`, cls: 'text-iso-warn' }
  const days = Math.floor(hrs / 24)
  return { text: `${days}d ago`, cls: 'text-iso-error' }
}

const lastSeen = computed(() => lastSeenRelative(props.host.last_seen_at))
const fingerprintShort = computed(() => {
  const fp = props.host.fingerprint
  if (!fp) return ''
  // Concept: `sha256:cb91…b2d`. If the fingerprint already includes a prefix
  // (e.g. `sha256:abcdef…`), keep it; otherwise drop into a short ellipsis.
  if (fp.length <= 24) return fp
  return `${fp.slice(0, 12)}…${fp.slice(-3)}`
})
</script>

<template>
  <div class="fixed inset-0 z-40 flex justify-end" @click.self="$emit('close')">
    <div class="absolute inset-0 bg-black/30"></div>
    <aside
      class="relative z-10 w-[480px] h-full bg-iso-bg-elevated border-l border-iso-border-subtle shadow-2xl flex flex-col overflow-hidden"
    >
      <!-- Header: status dot + hostname (mono) + close -->
      <header class="flex items-center justify-between px-5 py-4 border-b border-iso-border-subtle shrink-0">
        <div class="flex items-center gap-2.5 min-w-0">
          <span class="w-2 h-2 rounded-full bg-iso-success shrink-0"></span>
          <h2 class="font-mono text-base text-iso-text-primary truncate">{{ host.hostname }}</h2>
        </div>
        <button
          class="text-iso-text-muted hover:text-iso-text-primary transition-colors"
          aria-label="Close"
          @click="$emit('close')"
        >
          <Icon name="lucide:x" class="w-4 h-4" />
        </button>
      </header>

      <div class="flex-1 px-5 py-5 flex flex-col gap-6 overflow-y-auto">
        <!-- Identity (concept: id / arch / agent / docker / enrolled / last seen / fingerprint) -->
        <section class="flex flex-col gap-2">
          <div class="text-[10px] font-semibold tracking-wider text-iso-text-muted">IDENTITY</div>
          <dl class="grid grid-cols-[100px_1fr] gap-y-1.5 text-[11px]">
            <dt class="text-iso-text-muted">id</dt>
            <dd class="font-mono text-iso-text-secondary truncate">{{ host.id }}</dd>
            <template v-if="host.arch">
              <dt class="text-iso-text-muted">arch</dt>
              <dd class="font-mono text-iso-text-secondary">{{ host.arch }}</dd>
            </template>
            <template v-if="host.agent_version">
              <dt class="text-iso-text-muted">agent</dt>
              <dd class="font-mono text-iso-text-secondary">{{ host.agent_version }}</dd>
            </template>
            <template v-if="host.docker_version">
              <dt class="text-iso-text-muted">docker</dt>
              <dd class="font-mono text-iso-text-secondary">{{ host.docker_version }}</dd>
            </template>
            <template v-if="host.os">
              <dt class="text-iso-text-muted">os</dt>
              <dd class="font-mono text-iso-text-secondary">{{ host.os }}</dd>
            </template>
            <dt class="text-iso-text-muted">enrolled</dt>
            <dd class="font-mono text-iso-text-secondary">{{ formatTs(host.enrolled_at) }}</dd>
            <dt class="text-iso-text-muted">last seen</dt>
            <dd class="font-mono" :class="lastSeen.cls">{{ lastSeen.text }}</dd>
            <template v-if="host.fingerprint">
              <dt class="text-iso-text-muted">fingerprint</dt>
              <dd class="font-mono text-iso-text-secondary truncate" :title="host.fingerprint">
                {{ fingerprintShort }}
              </dd>
            </template>
          </dl>
        </section>

        <!-- kill-fleets: the per-host FLEET edit section is gone. Operators
             now express grouping via agent labels (agent.toml `[labels]`)
             and placement selectors instead. -->

        <!-- Settings: matches concept's "Force update cycle / Open shell / Decommission".
             Adds View stacks + Revoke cert (kept from existing flow; both useful). -->
        <section class="flex flex-col gap-2">
          <div class="text-[10px] font-semibold tracking-wider text-iso-text-muted">SETTINGS</div>
          <button
            class="px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle hover:border-iso-border-strong text-xs text-iso-text-secondary text-left flex items-center gap-2 transition-colors"
            @click="forceUpdate"
          >
            <Icon name="lucide:refresh-cw" class="w-3.5 h-3.5" />
            Force update cycle
          </button>
          <NuxtLink
            :to="`/stacks?host_id=${host.id}`"
            class="px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle hover:border-iso-border-strong text-xs text-iso-text-secondary text-left flex items-center gap-2 transition-colors"
          >
            <Icon name="lucide:layers" class="w-3.5 h-3.5" />
            View stacks on this host
          </NuxtLink>
          <button
            class="px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle text-xs text-iso-text-faint text-left flex items-center gap-2 cursor-not-allowed"
            disabled
            title="Coming soon"
          >
            <Icon name="lucide:terminal" class="w-3.5 h-3.5" />
            Open shell
            <span class="ml-auto text-[10px] text-iso-text-faint">soon</span>
          </button>
        </section>

        <!-- Danger zone: revoke cert + decommission. -->
        <section class="flex flex-col gap-2">
          <div class="text-[10px] font-semibold tracking-wider text-iso-error/80">DANGER ZONE</div>
          <button
            class="px-3 py-2 rounded-iso-md bg-iso-error/5 border border-iso-error/40 hover:bg-iso-error/10 hover:border-iso-error text-xs text-iso-error text-left flex items-center gap-2 transition-colors"
            @click="revokeCert"
          >
            <Icon name="lucide:shield-off" class="w-3.5 h-3.5" />
            Revoke cert
          </button>
          <button
            class="px-3 py-2 rounded-iso-md bg-iso-error/10 border border-iso-error hover:bg-iso-error/20 text-xs text-iso-error text-left flex items-center gap-2 transition-colors"
            @click="decommission"
          >
            <Icon name="lucide:trash-2" class="w-3.5 h-3.5" />
            Decommission
          </button>
        </section>
      </div>
    </aside>
  </div>
</template>
