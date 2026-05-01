<script setup lang="ts">
interface Props {
  buckets: number[]
  range: '24h' | '7d'
  totalEvents: number
}

defineProps<Props>()
defineEmits<{ 'range-change': [range: '24h' | '7d'] }>()
</script>

<template>
  <div class="flex items-center gap-4 px-4 py-3 border-b border-iso-border-subtle bg-iso-bg-elevated/30">
    <span class="text-xs text-iso-text-faint uppercase tracking-wider">Fleet weather</span>
    <Sparkline :data="buckets" color="success" :width="600" :height="28" />
    <span class="text-xs text-iso-text-muted font-mono">
      {{ totalEvents }} events / {{ range }}
    </span>
    <div class="ml-auto flex items-center gap-1">
      <button
        class="text-xs px-2 py-0.5 rounded hover:bg-iso-bg-base"
        :class="range === '24h' ? 'text-iso-text-primary bg-iso-bg-base' : 'text-iso-text-muted'"
        @click="$emit('range-change', '24h')"
      >24h</button>
      <button
        class="text-xs px-2 py-0.5 rounded hover:bg-iso-bg-base"
        :class="range === '7d' ? 'text-iso-text-primary bg-iso-bg-base' : 'text-iso-text-muted'"
        @click="$emit('range-change', '7d')"
      >7d</button>
    </div>
  </div>
</template>
