<template>
  <div class="flex items-center gap-2.5 py-1.5 border-b border-iso-border-subtle last:border-b-0 text-sm">
    <div
      class="w-2 h-2 rounded-full shrink-0"
      :style="{ backgroundColor: dotColor, boxShadow: `0 0 6px ${dotShadow}` }"
    ></div>
    <span class="text-iso-text-primary truncate text-xs">
      {{ issue.container_name }}
      <span v-if="issue.host_name" class="text-iso-text-faint font-mono text-[11px] ml-1">on {{ issue.host_name }}</span>
    </span>
    <div class="flex-1"></div>
    <span class="text-iso-text-muted font-mono text-[11px] truncate">{{ issue.detail }}</span>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

interface Issue {
  id: string | number
  container_name: string
  host_name: string
  detail: string
  state: 'healthy' | 'updating' | 'failed'
}

const props = defineProps<{
  issue: Issue
}>()

const dotColor = computed(() => {
  if (props.issue.state === 'failed') return '#f87171'
  if (props.issue.state === 'updating') return '#fbbf24'
  return '#4ade80'
})

const dotShadow = computed(() => {
  if (props.issue.state === 'failed') return '#f87171a0'
  if (props.issue.state === 'updating') return '#fbbf24a0'
  return '#4ade80a0'
})
</script>
