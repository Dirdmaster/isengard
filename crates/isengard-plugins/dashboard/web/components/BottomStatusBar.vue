<template>
  <div class="h-9 border-t border-iso-border-subtle bg-iso-bg-elevated px-4 flex items-center gap-4 text-iso-sm shrink-0">
    <span class="flex items-center gap-2" :class="stateLabel.color">
      <span class="w-2 h-2 rounded-full" :class="stateLabel.dot"></span>
      {{ stateLabel.text }}
    </span>
    <div class="flex-1"></div>
    <span class="text-iso-text-muted font-mono">⌘K command · ? help</span>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  connectionState: 'connecting' | 'live' | 'reconnecting' | 'offline'
  eventCount: number
}>()

const stateLabel = computed(() => {
  const liveText = props.eventCount === 0
    ? 'live'
    : `live: ${props.eventCount} ${props.eventCount === 1 ? 'event' : 'events'} today`
  const map: Record<string, { dot: string; text: string; color: string }> = {
    connecting:   { dot: 'bg-iso-warn',    text: 'connecting…',  color: 'text-iso-warn' },
    live:         { dot: 'bg-iso-success', text: liveText,       color: 'text-iso-success' },
    reconnecting: { dot: 'bg-iso-warn',    text: 'reconnecting…', color: 'text-iso-warn' },
    offline:      { dot: 'bg-iso-error',   text: 'offline',      color: 'text-iso-error' },
  }
  return map[props.connectionState]
})
</script>
