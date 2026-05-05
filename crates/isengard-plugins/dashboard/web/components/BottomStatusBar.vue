<template>
  <div class="h-11 border-t border-iso-border-subtle bg-iso-bg-elevated px-4 flex items-center gap-4 text-xs shrink-0">
    <!-- Left: connection state + event count -->
    <span class="flex items-center gap-2 font-medium" :class="stateLabel.color">
      <span class="w-2 h-2 rounded-full" :class="stateLabel.dot"></span>
      <span>{{ stateLabel.text }}</span>
    </span>

    <span class="text-iso-text-faint">·</span>

    <span class="font-mono text-iso-text-muted">
      controller v{{ version }}
    </span>

    <div class="flex-1"></div>

    <!-- Right: real interactive keyboard affordances -->
    <div class="flex items-center gap-2">
      <button
        type="button"
        class="flex items-center gap-1.5 px-2 py-1 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle text-iso-text-secondary hover:border-iso-border-strong hover:text-iso-text-primary transition-colors"
        title="Open command pane"
        @click="ui.openCmdPane('navigator')"
      >
        <span class="font-mono text-[11px]">⌘K</span>
        <span class="text-[11px] text-iso-text-muted">command</span>
      </button>
      <button
        type="button"
        class="flex items-center gap-1.5 px-2 py-1 rounded-iso-sm bg-iso-bg-base border border-iso-border-subtle text-iso-text-secondary hover:border-iso-border-strong hover:text-iso-text-primary transition-colors"
        title="Show keyboard shortcuts"
        @click="ui.helpOpen = true"
      >
        <span class="font-mono text-[11px]">?</span>
        <span class="text-[11px] text-iso-text-muted">help</span>
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  connectionState: 'connecting' | 'live' | 'reconnecting' | 'offline'
  eventCount: number
}>()

const ui = useUiStore()

// Workspace version. Pulled from the dashboard package — kept in sync with the
// Cargo workspace by ops, so it's a fine proxy for "controller version" until
// the controller surfaces its own /api/version.
const version = '0.1.0-alpha'

const stateLabel = computed(() => {
  const liveText = props.eventCount === 0
    ? 'live'
    : `live · ${props.eventCount} ${props.eventCount === 1 ? 'event' : 'events'} today`
  const map: Record<string, { dot: string; text: string; color: string }> = {
    connecting:   { dot: 'bg-iso-warn',    text: 'connecting…',  color: 'text-iso-warn' },
    live:         { dot: 'bg-iso-success', text: liveText,       color: 'text-iso-success' },
    reconnecting: { dot: 'bg-iso-warn',    text: 'reconnecting…', color: 'text-iso-warn' },
    offline:      { dot: 'bg-iso-error',   text: 'offline',      color: 'text-iso-error' },
  }
  return map[props.connectionState]
})
</script>
