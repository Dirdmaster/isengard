<template>
  <div class="grid grid-cols-[90px_1fr] gap-3 text-iso-sm">
    <span class="text-iso-text-faint">{{ label }}</span>
    <span :class="[mono ? 'font-mono text-iso-xs' : '', valueClass ?? 'text-iso-text-secondary']">
      {{ value }}
    </span>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  label: string
  value: string
  mono?: boolean
  valueClass?: string
}>()
</script>
