<template>
  <button
    class="w-full grid grid-cols-[80px_180px_1fr_140px] gap-3 px-4 py-2.5 text-xs items-center text-left border-b border-iso-border-subtle transition-colors"
    :class="selected ? 'bg-iso-bg-selected' : 'hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
  >
    <span class="font-mono text-iso-text-faint">{{ formatTime(event.occurred_at) }}</span>
    <span>
      <span
        class="px-1.5 py-px rounded-iso-sm font-mono text-[10px] truncate inline-block max-w-full"
        :class="chipClass"
      >{{ event.kind }}</span>
    </span>
    <span class="text-iso-text-secondary truncate">{{ message }}</span>
    <span class="font-mono text-iso-info text-[11px] truncate">{{ target }}</span>
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

// Map event.kind → tone family. Mirrors the concept's chip palette.
// Same logic as ActivityCard so /events and the home feed agree.
function toneFor(kind: string): 'success' | 'warn' | 'error' | 'info' | 'neutral' {
  if (
    kind.startsWith('update.success') ||
    kind.startsWith('routing.healthy') ||
    kind.startsWith('backup.success') ||
    kind.startsWith('policy.evaluated') ||
    kind.startsWith('webhook.delivered')
  ) return 'success'
  if (
    kind.startsWith('update.failed') ||
    kind.startsWith('routing.degraded') ||
    kind.startsWith('healthcheck.failed') ||
    kind.startsWith('approval.rejected')
  ) return 'error'
  if (
    kind.startsWith('update.pending_approval') ||
    kind.startsWith('update.pulling') ||
    kind.startsWith('approval.pending')
  ) return 'warn'
  if (
    kind.startsWith('deploy.') ||
    kind.startsWith('agent.') ||
    kind.startsWith('webhook.') ||
    kind.startsWith('hooks.') ||
    kind.startsWith('stack.')
  ) return 'info'
  return 'neutral'
}

const chipClass = computed(() => {
  switch (toneFor(props.event.kind)) {
    case 'success': return 'bg-iso-success-soft text-iso-success'
    case 'warn':    return 'bg-iso-warn-soft text-iso-warn'
    case 'error':   return 'bg-iso-error-soft text-iso-error'
    case 'info':    return 'bg-iso-info-soft text-iso-info'
    default:        return 'bg-iso-bg-overlay text-iso-text-muted'
  }
})

// Concept "MESSAGE" column: just the human-readable summary, lightly enriched
// with container if the summary doesn't already mention it. Falls back gracefully.
const message = computed(() => {
  const e = props.event
  const summary = e.summary?.trim()
  const container = e.container_name?.trim()
  if (summary && container && !summary.toLowerCase().includes(container.toLowerCase())) {
    return `${container} ${summary}`
  }
  return summary || container || e.kind
})

// Concept "TARGET" column: `host / service` (or em-dash placeholder).
const target = computed(() => {
  const e = props.event
  const host = e.host_id ? e.host_id.slice(0, 8) : '—'
  const svc  = e.container_name ?? '—'
  return `${host} / ${svc}`
})

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString([], { hour12: false })
}
</script>
