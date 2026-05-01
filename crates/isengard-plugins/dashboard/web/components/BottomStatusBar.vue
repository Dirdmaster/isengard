<template>
  <div class="h-8 border-t border-iso-border-subtle bg-iso-bg-elevated px-4 flex items-center gap-4 text-iso-xs">
    <div class="flex items-center gap-1.5">
      <div class="w-1.5 h-1.5 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-error'"></div>
      <span :class="connected ? 'text-iso-success' : 'text-iso-error'">
        {{ connected ? `live · ${eventCount} events today` : 'disconnected' }}
      </span>
    </div>
    <span class="text-iso-text-faint">controller v0.1.0-alpha</span>
    <div class="flex-1"></div>
    <span class="text-iso-text-faint font-mono">↑↓ navigate · ⌘K command · ? help</span>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  connected: boolean
  eventCount: number
}>()
</script>
