<template>
  <header class="flex items-center justify-between px-4 py-3 border-b border-iso-border-subtle shrink-0">
    <div class="flex items-center gap-3 min-w-0">
      <h1 class="font-mono text-base text-iso-text-primary shrink-0">{{ title }}</h1>
      <span v-if="subtitle" class="text-xs text-iso-text-muted truncate">{{ subtitle }}</span>
      <slot name="meta" />
    </div>
    <div class="flex items-center gap-2 shrink-0">
      <slot name="actions" />
    </div>
  </header>
</template>

<script setup lang="ts">
defineProps<{
  title: string
  subtitle?: string
}>()
</script>
