<script setup lang="ts">
import { onMounted, onBeforeUnmount, ref, watch, computed } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { useLogStream } from '~/composables/useLogStream'

interface Props {
  serviceId: string
  serviceName: string
  hostHostname: string
  fleet: string
  stackName?: string
}

const props = defineProps<Props>()
defineEmits<{ 'toggle-position': []; close: [] }>()

const containerEl = ref<HTMLElement | null>(null)
let term: Terminal | null = null
let fit: FitAddon | null = null

const { lines, message, state, connect, disconnect } = useLogStream(props.serviceId)
const connected = computed(() => state.value === 'connected')

onMounted(() => {
  if (!containerEl.value) return
  term = new Terminal({
    fontFamily: 'JetBrains Mono, monospace',
    fontSize: 13,
    theme: {
      background: '#0a0e14',
      foreground: '#c5c8c6',
      cursor: '#7fdbca',
    },
    convertEol: true,
  })
  fit = new FitAddon()
  term.loadAddon(fit)
  term.open(containerEl.value)
  fit.fit()
  connect()
})

onBeforeUnmount(() => {
  disconnect()
  term?.dispose()
})

// Append new lines to xterm.
watch(lines, (newLines) => {
  if (!term) return
  const last = newLines[newLines.length - 1]
  if (last) {
    const color = last.stream === 'stderr' ? '\x1b[31m' : ''
    const reset = last.stream === 'stderr' ? '\x1b[0m' : ''
    term.writeln(`${color}${last.text}${reset}`)
  }
}, { deep: false })

// Render the v1 placeholder info/error frame from the WS as a yellow line.
watch(message, (msg) => {
  if (!term || !msg) return
  const color = msg.type === 'error' ? '\x1b[31m' : '\x1b[33m'
  term.writeln(`${color}[${msg.type}] ${msg.message}\x1b[0m`)
}, { immediate: false })

const breadcrumb = computed(() => [
  'isengard',
  props.fleet,
  ...(props.stackName ? [props.stackName] : []),
  `${props.serviceName} @ ${props.hostHostname}`,
  'logs',
])
</script>

<template>
  <div class="flex flex-col h-full">
    <CmdBreadcrumb
      :segments="breadcrumb"
      :connected="connected"
      @toggle-position="$emit('toggle-position')"
      @close="$emit('close')"
    />
    <div ref="containerEl" class="flex-1 overflow-hidden bg-[#0a0e14]"></div>
    <footer class="px-3 py-1.5 border-t border-iso-border-subtle bg-iso-bg-elevated text-[10px] text-iso-text-faint font-mono">
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘P</kbd> navigator ·
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘W</kbd> close ·
      <kbd class="px-1 py-0.5 bg-iso-bg-base rounded">⌘.</kbd> toggle position
    </footer>
  </div>
</template>
