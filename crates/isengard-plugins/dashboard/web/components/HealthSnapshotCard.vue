<script setup lang="ts">
import { computed } from 'vue'

/**
 * Health snapshot card — fleet-wide pulse summary.
 *
 * Per `design/concepts/home/v1.html` (the "Fleet pulse" panel — renamed
 * "Health snapshot" per the brief). Each row is a label + mono value pair.
 *
 * Derived from the existing stores. Some values are placeholders until the
 * backing data lands (webhook stats, last backup) — show "—" rather than
 * fabricate.
 */

const hostsStore = useHostsStore()
const eventsStore = useEventsStore()

const hostsTotal = computed(() => hostsStore.hosts.length)

const hostsUp = computed(() => {
  const cutoff = Date.now() - 5 * 60 * 1000
  return hostsStore.hosts.filter((h) => {
    if (!h.last_seen_at) return false
    return new Date(h.last_seen_at).getTime() >= cutoff
  }).length
})

const hostsStale = computed(() => hostsTotal.value - hostsUp.value)

const lastEventAgo = computed(() => {
  const e = eventsStore.events[0]
  if (!e) return null
  const ms = Date.now() - new Date(e.occurred_at).getTime()
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
})

// Group hosts by fleet, listing the top fleets so the user sees distribution.
const fleetSummary = computed(() => {
  const counts = new Map<string, { up: number; total: number }>()
  const cutoff = Date.now() - 5 * 60 * 1000
  for (const h of hostsStore.hosts) {
    const c = counts.get(h.fleet) ?? { up: 0, total: 0 }
    c.total++
    if (h.last_seen_at && new Date(h.last_seen_at).getTime() >= cutoff) c.up++
    counts.set(h.fleet, c)
  }
  return Array.from(counts.entries())
    .sort((a, b) => b[1].total - a[1].total)
    .slice(0, 3)
})
</script>

<template>
  <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated flex flex-col overflow-hidden flex-1 min-h-0">
    <div class="px-4 py-3 border-b border-iso-border-subtle">
      <span class="text-iso-xs font-semibold text-iso-text-primary">Health snapshot</span>
    </div>
    <div class="p-4 flex flex-col gap-3 text-iso-xs">
      <div class="flex items-center justify-between">
        <span class="text-iso-text-muted">Hosts</span>
        <span
          class="font-mono"
          :class="hostsTotal > 0 && hostsStale === 0 ? 'text-iso-success' : 'text-iso-text-secondary'"
        >
          {{ hostsUp }} / {{ hostsTotal }} reporting
        </span>
      </div>

      <div v-if="hostsStale > 0" class="flex items-center justify-between">
        <span class="text-iso-text-muted">Stale agents</span>
        <span class="font-mono text-iso-warn">{{ hostsStale }} silent &gt;5m</span>
      </div>

      <div
        v-for="[name, count] in fleetSummary"
        :key="name"
        class="flex items-center justify-between"
      >
        <span class="text-iso-text-muted">Fleet · <span class="font-mono">{{ name }}</span></span>
        <span class="font-mono text-iso-text-secondary">{{ count.up }} / {{ count.total }} up</span>
      </div>

      <div class="flex items-center justify-between">
        <span class="text-iso-text-muted">Last event</span>
        <span class="font-mono text-iso-text-secondary">
          {{ lastEventAgo ?? '—' }}
        </span>
      </div>
    </div>
  </div>
</template>
