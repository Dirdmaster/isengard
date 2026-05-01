<template>
  <button
    class="w-full grid grid-cols-[60px_90px_1fr_auto] gap-3.5 px-5 py-2 items-center text-left transition-colors border-l-2"
    :class="selected ? 'bg-iso-bg-selected border-iso-success' : 'border-transparent hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
  >
    <span class="font-mono text-iso-xs text-iso-text-faint">{{ formatTime(event.occurred_at) }}</span>
    <span class="font-mono text-iso-xs font-medium" :class="kindClass">{{ kindLabel }}</span>
    <span class="text-iso-base text-iso-text-secondary truncate">
      <span v-if="event.container_name" class="font-medium text-iso-text-primary">{{ event.container_name }}</span>
      <span v-if="event.summary" class="ml-1">{{ event.summary }}</span>
    </span>
    <span class="text-iso-xs text-iso-text-faint font-mono">{{ shortHostId }}</span>
  </button>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { EventRow as EventType } from '~/stores/events'

const props = defineProps<{
  event: EventType
  selected: boolean
}>()
defineEmits<{ select: [] }>()

const kindLabel = computed(() => props.event.kind.split('.')[1]?.toUpperCase() ?? props.event.kind.toUpperCase())

const kindClass = computed(() => {
  const k = props.event.kind
  if (k.startsWith('update.success')) return 'text-iso-success'
  if (k.startsWith('update.failed')) return 'text-iso-error'
  if (k.startsWith('update.pulling')) return 'text-iso-warn'
  if (k.startsWith('update.checked')) return 'text-iso-neutral'
  if (k.startsWith('agent.disconnect')) return 'text-iso-info'
  return 'text-iso-neutral'
})

const shortHostId = computed(() => {
  if (!props.event.host_id) return ''
  return props.event.host_id.slice(0, 6)
})

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString([], { hour12: false })
}
</script>
