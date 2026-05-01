<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  hosts: Host[]
  sparklines: Record<string, number[]>
  stackCounts: Record<string, { stacks: number; services: number }>
  latestEvents: Record<string, { kind: string; summary: string } | null>
  selectedId: string | null
}

defineProps<Props>()
const emit = defineEmits<{
  select: [host: Host]
  action: [action: 'force-update' | 'shell' | 'menu', host: Host]
}>()

function lastSeenRelative(host: Host): string {
  if (!host.last_seen_at) return 'never'
  const ms = Date.now() - new Date(host.last_seen_at).getTime()
  const mins = Math.floor(ms / 60000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h ago`
  const days = Math.floor(hrs / 24)
  return `${days}d ago`
}
</script>

<template>
  <div class="flex flex-col min-h-0">
    <div
      v-show="hosts.length > 0"
      class="grid items-center gap-3 px-3 py-2 text-[10px] uppercase tracking-wider text-iso-text-faint border-b border-iso-border-subtle shrink-0"
      style="grid-template-columns: 170px 70px 130px 80px 1fr 90px 60px auto"
    >
      <span>Host</span>
      <span>Fleet</span>
      <span>Activity</span>
      <span>Stacks</span>
      <span>Latest</span>
      <span>Last seen</span>
      <span>Agent</span>
      <span></span>
    </div>
    <EmptyState
      v-if="hosts.length === 0"
      icon="server"
      title="No hosts yet"
      description="Add your first host to start tracking containers across your fleet."
    />

    <template v-else>
      <HostRow
        v-for="h in hosts"
        :key="h.id"
        :host="h"
        :sparkline="sparklines[h.id] ?? []"
        :stack-count="stackCounts[h.id]?.stacks ?? 0"
        :service-count="stackCounts[h.id]?.services ?? 0"
        :latest-event="latestEvents[h.id] ?? null"
        :last-seen-relative="lastSeenRelative(h)"
        :agent-version-warn="false"
        :selected="selectedId === h.id"
        @click="emit('select', h)"
        @action="(a, host) => emit('action', a, host)"
      />
    </template>
  </div>
</template>
