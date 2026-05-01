<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  host: Host
  sparkline: number[]
  stackCount: number
  serviceCount: number
  latestEvent: { kind: string; summary: string } | null
  lastSeenRelative: string
  agentVersionWarn: boolean
  selected?: boolean
}

const props = defineProps<Props>()
const emit = defineEmits<{
  click: [host: Host]
  action: [action: 'force-update' | 'shell' | 'menu', host: Host]
}>()

const stateDot = computed((): string => {
  const kind = props.latestEvent?.kind
  if (kind === 'FAILED' || kind === 'update.failed') return 'bg-iso-error'
  if (kind === 'PULLING' || kind === 'update.pulling') return 'bg-iso-warn'
  if (kind === 'DISCONNECT' || kind === 'agent.disconnect_long') return 'bg-iso-info'
  return 'bg-iso-success'
})

const kindColor = (kind: string) => ({
  UPDATED:    'text-iso-success',
  FAILED:     'text-iso-error',
  CHECKED:    'text-iso-text-muted',
  PULLING:    'text-iso-warn',
  DISCONNECT: 'text-iso-info',
}[kind] ?? 'text-iso-text-muted')
</script>

<template>
  <div
    class="group grid items-center gap-3 px-3 py-2 hover:bg-iso-bg-elevated cursor-pointer border-l-2"
    :class="selected ? 'border-iso-success bg-iso-success/5' : 'border-transparent'"
    style="grid-template-columns: 170px 70px 130px 80px 1fr 90px 60px auto"
    @click="emit('click', host)"
  >
    <div class="flex items-center gap-2 min-w-0">
      <span class="w-2 h-2 rounded-full shrink-0" :class="stateDot"></span>
      <span class="font-mono text-sm truncate">{{ host.hostname }}</span>
    </div>
    <span class="text-xs text-iso-text-muted">{{ host.fleet }}</span>
    <Sparkline :data="sparkline" color="success" :width="120" :height="20" />
    <span class="text-xs text-iso-text-muted font-mono">
      {{ stackCount }} · {{ serviceCount }} svcs
    </span>
    <span v-if="latestEvent" class="text-xs font-mono truncate">
      <span :class="kindColor(latestEvent.kind)">{{ latestEvent.kind }}</span>
      <span class="text-iso-text-muted ml-1">{{ latestEvent.summary }}</span>
    </span>
    <span v-else class="text-xs text-iso-text-faint">no events</span>
    <span class="text-xs text-iso-text-muted">{{ lastSeenRelative }}</span>
    <span
      class="text-xs font-mono"
      :class="agentVersionWarn ? 'text-iso-warn' : 'text-iso-text-muted'"
    >
      {{ host.agent_version }}
    </span>
    <div class="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        title="Force update"
        @click.stop="emit('action', 'force-update', host)"
      >
        <Icon name="lucide:zap" class="w-3.5 h-3.5" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        title="Open shell"
        @click.stop="emit('action', 'shell', host)"
      >
        <Icon name="lucide:terminal" class="w-3.5 h-3.5" />
      </button>
      <button
        class="p-1 rounded hover:bg-iso-bg-base"
        title="More"
        @click.stop="emit('action', 'menu', host)"
      >
        <Icon name="lucide:ellipsis" class="w-3.5 h-3.5" />
      </button>
    </div>
  </div>
</template>
