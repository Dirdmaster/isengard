<script setup lang="ts">
import { computed } from 'vue'
import type { Host } from '~/stores/hosts'

interface Props {
  host: Host
  stackCount: number
  serviceCount: number
  latestEvent: { kind: string; summary: string } | null
  lastSeenRelative: string
  agentVersionWarn: boolean
  selected?: boolean
  isLast?: boolean
}

const props = defineProps<Props>()
const emit = defineEmits<{
  click: [host: Host]
  action: [action: 'force-update' | 'shell' | 'menu', host: Host]
}>()

// Status dot color is driven by the most recent event for this host.
// Inlined per components.md decision: status dots stay un-extracted.
const stateDot = computed((): string => {
  const kind = props.latestEvent?.kind
  if (kind === 'FAILED' || kind === 'update.failed') return 'bg-iso-error'
  if (kind === 'PULLING' || kind === 'update.pulling') return 'bg-iso-warn'
  if (kind === 'DISCONNECT' || kind === 'agent.disconnect_long') return 'bg-iso-error'
  return 'bg-iso-success'
})

// Agent column shows `unreachable` in red if the host has gone dark.
const agentLabel = computed(() => {
  if (props.latestEvent?.kind === 'DISCONNECT' || props.latestEvent?.kind === 'agent.disconnect_long') {
    return { text: 'unreachable', cls: 'text-iso-error' }
  }
  return {
    text: props.host.agent_version ?? '—',
    cls: props.agentVersionWarn ? 'text-iso-warn' : 'text-iso-text-secondary',
  }
})

// Compose `Ubuntu 22.04 · 24.0.7` per concept v1 (`OS / DOCKER` column).
const osDocker = computed(() => {
  const parts: string[] = []
  if (props.host.os) parts.push(props.host.os)
  if (props.host.docker_version) parts.push(props.host.docker_version)
  return parts.join(' · ')
})
</script>

<template>
  <div
    class="group grid items-center px-4 py-3 text-xs hover:bg-iso-bg-base cursor-pointer"
    :class="[
      isLast ? '' : 'border-b border-iso-border-subtle',
      selected ? 'bg-iso-info-soft/40' : '',
    ]"
    style="grid-template-columns: 180px 120px 110px minmax(180px, 1fr) 120px 100px 80px"
    @click="emit('click', host)"
  >
    <div class="flex items-center gap-2 min-w-0">
      <span class="w-2 h-2 rounded-full shrink-0" :class="stateDot"></span>
      <span class="font-medium text-iso-text-primary truncate">{{ host.hostname }}</span>
    </div>
    <span class="text-iso-text-muted truncate">{{ host.fleet }}</span>
    <span class="text-iso-text-muted">
      {{ stackCount }} · {{ serviceCount }} svcs
    </span>
    <span
      v-if="osDocker"
      class="text-iso-text-secondary truncate"
      :title="osDocker"
    >{{ osDocker }}</span>
    <span v-else class="text-iso-text-faint">—</span>
    <span class="text-iso-text-muted">{{ lastSeenRelative }}</span>
    <span class="font-mono" :class="agentLabel.cls">{{ agentLabel.text }}</span>
    <div class="flex items-center justify-end gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
      <button
        class="p-1 rounded hover:bg-iso-bg-elevated text-iso-text-muted hover:text-iso-text-primary"
        title="Force update"
        @click.stop="emit('action', 'force-update', host)"
      >
        <Icon name="lucide:zap" class="w-3.5 h-3.5" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-elevated text-iso-text-muted hover:text-iso-text-primary"
        title="Open shell"
        @click.stop="emit('action', 'shell', host)"
      >
        <Icon name="lucide:terminal" class="w-3.5 h-3.5" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-elevated text-iso-text-muted hover:text-iso-text-primary"
        title="More"
        @click.stop="emit('action', 'menu', host)"
      >
        <Icon name="lucide:ellipsis" class="w-3.5 h-3.5" />
      </button>
    </div>
  </div>
</template>
