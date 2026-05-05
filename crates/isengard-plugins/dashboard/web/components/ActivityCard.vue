<script setup lang="ts">
import { computed } from 'vue'
import type { EventRow } from '~/stores/events'

/**
 * Recent activity card — flat reverse-chronological feed for the home page.
 *
 * Per `design/concepts/home/v1.html`: no day separators (unlike EventTimeline
 * on /events). Each row is `mono timestamp w-12 | kind chip | text-secondary
 * description`. Header has a live dot/label.
 */

const eventsStore = useEventsStore()
const hostsStore = useHostsStore()

const events = computed(() => eventsStore.events.slice(0, 30))

function timeOf(iso: string): string {
  const d = new Date(iso)
  return d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit' })
}

function hostnameOf(hostId: string | null): string {
  if (!hostId) return ''
  const h = hostsStore.hosts.find(x => x.id === hostId)
  return h?.hostname ?? hostId.slice(0, 6)
}

// Map a kind to one of: success | warn | error | info | neutral.
// Drives the chip's soft-bg + text color combo per the concept.
function toneFor(kind: string): 'success' | 'warn' | 'error' | 'info' | 'neutral' {
  if (kind.startsWith('update.success') || kind.startsWith('routing.healthy') || kind.startsWith('backup.success') || kind.startsWith('policy.evaluated')) return 'success'
  if (kind.startsWith('update.failed') || kind.startsWith('routing.degraded')) return 'error'
  if (kind.startsWith('update.pending_approval') || kind.startsWith('update.pulling')) return 'warn'
  if (kind.startsWith('deploy.') || kind.startsWith('agent.heartbeat') || kind.startsWith('webhook.') || kind.startsWith('agent.disconnect')) return 'info'
  return 'neutral'
}

function chipClasses(kind: string): string {
  switch (toneFor(kind)) {
    case 'success': return 'bg-iso-success-soft text-iso-success'
    case 'warn':    return 'bg-iso-warn-soft text-iso-warn'
    case 'error':   return 'bg-iso-error-soft text-iso-error'
    case 'info':    return 'bg-iso-info-soft text-iso-info'
    default:        return 'bg-iso-bg-overlay text-iso-text-muted'
  }
}

function descriptionOf(e: EventRow): string {
  const host = hostnameOf(e.host_id)
  const container = e.container_name
  const summary = e.summary
  // Concept patterns: "web-app · prod-01 v2.4.0 → v2.4.1"
  // We don't always have all parts — fall back to summary.
  if (container && host && summary) return `${container} · ${host} ${summary}`
  if (container && host) return `${container} · ${host}`
  if (container && summary) return `${container} · ${summary}`
  if (host && summary) return `${host} · ${summary}`
  return summary || container || host || e.kind
}
</script>

<template>
  <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated flex flex-col overflow-hidden min-h-0">
    <div class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between shrink-0">
      <span class="text-iso-xs font-semibold text-iso-text-primary">Recent activity</span>
      <div class="flex items-center gap-2">
        <span class="w-1.5 h-1.5 rounded-full bg-iso-success animate-pulse"></span>
        <span class="text-[11px] text-iso-text-muted">live</span>
      </div>
    </div>
    <div class="flex-1 overflow-auto min-h-0">
      <div
        v-if="events.length === 0"
        class="px-4 py-8 text-center text-iso-text-faint text-iso-xs"
      >
        No events yet — activity will stream in as Isengard checks for image updates.
      </div>
      <div
        v-for="(e, idx) in events"
        :key="e.id"
        class="px-4 py-2.5 flex items-center gap-3 text-iso-xs"
        :class="idx < events.length - 1 ? 'border-b border-iso-border-subtle' : ''"
      >
        <span class="font-mono text-iso-text-faint w-12 shrink-0">{{ timeOf(e.occurred_at) }}</span>
        <span
          class="px-1.5 py-px rounded-iso-sm font-mono text-[10px] shrink-0 truncate max-w-[160px]"
          :class="chipClasses(e.kind)"
        >{{ e.kind }}</span>
        <span class="text-iso-text-secondary truncate flex-1">{{ descriptionOf(e) }}</span>
      </div>
    </div>
  </div>
</template>
