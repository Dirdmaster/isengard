<script setup lang="ts">
interface Props {
  name: string
  state?: 'running' | 'stopped' | 'restarting' | 'unknown'
}

const props = withDefaults(defineProps<Props>(), {
  state: 'unknown',
})

const dotColor = computed(() => ({
  running:    'bg-iso-success',
  stopped:    'bg-iso-text-faint',
  restarting: 'bg-iso-warn',
  unknown:    'bg-iso-text-muted',
})[props.state])
</script>

<template>
  <span class="inline-flex items-center gap-1.5 rounded px-2 py-1 bg-iso-bg-elevated border border-iso-border-subtle text-xs font-mono">
    <span class="w-1.5 h-1.5 rounded-full" :class="dotColor"></span>
    {{ name }}
  </span>
</template>
