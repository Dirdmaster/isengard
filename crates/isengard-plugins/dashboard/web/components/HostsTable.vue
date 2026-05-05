<script setup lang="ts">
import type { Host } from '~/stores/hosts'

interface Props {
  hosts: Host[]
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
  const secs = Math.floor(ms / 1000)
  if (secs < 60) return `${Math.max(secs, 1)}s ago`
  const mins = Math.floor(secs / 60)
  if (mins < 60) return `${mins}m ago`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h ago`
  const days = Math.floor(hrs / 24)
  return `${days}d ago`
}

// Concept v1 (`design/concepts/hosts/v1.html`) renders the table inside a
// rounded-iso-lg card with elevated bg + iso-border-subtle. Header row uses
// uppercase 10px tracking-wider muted labels.
</script>

<template>
  <div class="flex flex-col min-h-0">
    <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden flex flex-col min-h-0">
      <div
        v-show="hosts.length > 0"
        class="grid items-center px-4 py-2.5 text-[10px] font-semibold tracking-wider text-iso-text-muted border-b border-iso-border-subtle shrink-0"
        style="grid-template-columns: 180px 120px 110px minmax(180px, 1fr) 120px 100px 80px"
      >
        <div>HOSTNAME</div>
        <div>FLEET</div>
        <div>STACKS</div>
        <div>OS / DOCKER</div>
        <div>LAST SEEN</div>
        <div>AGENT</div>
        <div></div>
      </div>

      <EmptyState
        v-if="hosts.length === 0"
        icon="server"
        title="No hosts enrolled yet"
        description="A host is any machine running Docker that you want Isengard to manage. Run one command on each host and it appears here."
      >
        <template #cta>
          <AddHostButton />
        </template>
      </EmptyState>

      <div v-else class="flex flex-col min-h-0 overflow-y-auto">
        <HostRow
          v-for="(h, i) in hosts"
          :key="h.id"
          :host="h"
          :stack-count="stackCounts[h.id]?.stacks ?? 0"
          :service-count="stackCounts[h.id]?.services ?? 0"
          :latest-event="latestEvents[h.id] ?? null"
          :last-seen-relative="lastSeenRelative(h)"
          :agent-version-warn="false"
          :selected="selectedId === h.id"
          :is-last="i === hosts.length - 1"
          @click="emit('select', h)"
          @action="(a, host) => emit('action', a, host)"
        />
      </div>
    </div>
  </div>
</template>
