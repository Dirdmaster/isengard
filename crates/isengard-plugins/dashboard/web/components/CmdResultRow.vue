<template>
  <button
    class="w-full flex items-center gap-3 px-5 py-2 transition-colors text-left"
    :class="highlighted ? 'bg-iso-bg-row-hover' : 'hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
  >
    <Icon :name="icon" class="w-3.5 h-3.5 text-iso-text-muted" />
    <span class="text-iso-base" :class="highlighted ? 'text-iso-text-primary font-medium' : 'text-iso-text-secondary'">{{ label }}</span>
    <span v-if="meta" class="text-iso-xs font-mono text-iso-text-faint">{{ meta }}</span>
    <div class="flex-1"></div>
    <kbd v-if="highlighted" class="px-1.5 py-0.5 rounded text-iso-xs font-mono border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">⏎ open</kbd>
  </button>
</template>

<script setup lang="ts">
defineProps<{
  icon: string
  label: string
  meta?: string
  highlighted?: boolean
}>()
defineEmits<{ select: [] }>()
</script>
