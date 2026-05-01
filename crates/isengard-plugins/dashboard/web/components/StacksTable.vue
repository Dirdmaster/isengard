<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Row {
  stack: Stack
  hostHostname: string
  fleet: string
  serviceCount: number
  latestEvent: { kind: string; summary: string } | null
}

interface Props {
  rows: Row[]
}

defineProps<Props>()

const router = useRouter()
const kindColor = (kind: string) => ({
  UPDATED:    'text-iso-success',
  FAILED:     'text-iso-error',
  CHECKED:    'text-iso-text-muted',
  PULLING:    'text-iso-warn',
  DISCONNECT: 'text-iso-info',
}[kind] ?? 'text-iso-text-muted')
</script>

<template>
  <div>
    <div
      class="grid items-center gap-3 px-3 py-2 text-[10px] uppercase tracking-wider text-iso-text-faint border-b border-iso-border-subtle"
      style="grid-template-columns: 200px 170px 70px 70px 1fr 90px"
    >
      <span>Stack</span>
      <span>Host</span>
      <span>Fleet</span>
      <span>Services</span>
      <span>Latest event</span>
      <span>Source</span>
    </div>

    <div v-if="rows.length === 0" class="py-16 text-center">
      <Icon name="lucide:layers" class="w-9 h-9 text-iso-text-faint mx-auto mb-3" />
      <p class="text-sm text-iso-text-muted mb-1">No stacks match the current filter</p>
      <p class="text-xs text-iso-text-faint max-w-md mx-auto">Stacks are discovered from the <code class="font-mono">com.docker.compose.project</code> label on running containers.</p>
    </div>

    <template v-else>
      <div
        v-for="row in rows"
        :key="row.stack.id"
        class="grid items-center gap-3 px-3 py-2 hover:bg-iso-bg-elevated cursor-pointer"
        style="grid-template-columns: 200px 170px 70px 70px 1fr 90px"
        @click="router.push(`/stacks/${row.stack.id}`)"
      >
        <div class="flex items-center gap-2 min-w-0">
          <Icon name="lucide:layers" class="w-3.5 h-3.5 text-iso-text-muted shrink-0" />
          <span class="font-mono text-sm truncate">{{ row.stack.name }}</span>
        </div>
        <span class="font-mono text-xs text-iso-text-muted truncate">{{ row.hostHostname }}</span>
        <span class="text-xs text-iso-text-muted">{{ row.fleet }}</span>
        <span class="text-xs text-iso-text-muted font-mono">{{ row.serviceCount }}</span>
        <span v-if="row.latestEvent" class="text-xs font-mono truncate">
          <span :class="kindColor(row.latestEvent.kind)">{{ row.latestEvent.kind }}</span>
          <span class="text-iso-text-muted ml-1">{{ row.latestEvent.summary }}</span>
        </span>
        <span v-else class="text-xs text-iso-text-faint">no events</span>
        <span class="text-xs text-iso-text-faint uppercase">{{ row.stack.source }}</span>
      </div>
    </template>
  </div>
</template>
