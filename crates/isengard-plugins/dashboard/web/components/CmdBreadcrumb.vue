<script setup lang="ts">
interface Props {
  segments: string[]
  connected: boolean
}
defineProps<Props>()
defineEmits<{ 'toggle-position': []; close: [] }>()
</script>

<template>
  <header class="flex items-center justify-between px-3 py-2 border-b border-iso-border-subtle bg-iso-bg-elevated">
    <div class="flex items-center gap-2 text-xs font-mono">
      <Icon name="lucide:terminal" class="w-3.5 h-3.5 text-iso-text-muted" />
      <span
        v-for="(seg, i) in segments"
        :key="i"
        class="flex items-center gap-1.5"
      >
        <span class="text-iso-text-muted">{{ seg }}</span>
        <span v-if="i < segments.length - 1" class="text-iso-text-faint">›</span>
      </span>
      <span class="ml-3 flex items-center gap-1.5">
        <span class="w-1.5 h-1.5 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-text-faint'"></span>
        <span class="text-iso-text-faint">{{ connected ? 'connected' : 'disconnected' }}</span>
      </span>
    </div>
    <div class="flex items-center gap-1">
      <button class="p-1 rounded hover:bg-iso-bg-base" title="Toggle position (⌘.)" @click="$emit('toggle-position')">
        <Icon name="lucide:arrows-up-down" class="w-3.5 h-3.5" />
      </button>
      <button class="p-1 rounded hover:bg-iso-bg-base" title="Close (⌘W)" @click="$emit('close')">
        <Icon name="lucide:x" class="w-3.5 h-3.5" />
      </button>
    </div>
  </header>
</template>
