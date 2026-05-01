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
  <div class="flex flex-col min-h-0">
    <div
      class="grid items-center gap-3 px-3 py-2 text-[10px] uppercase tracking-wider text-iso-text-faint border-b border-iso-border-subtle shrink-0"
      v-show="rows.length > 0"
      style="grid-template-columns: 200px 170px 70px 70px 1fr 90px"
    >
      <span>Stack</span>
      <span>Host</span>
      <span>Fleet</span>
      <span>Services</span>
      <span>Latest event</span>
      <span>Source</span>
    </div>

    <div v-if="rows.length === 0" class="flex-1 flex flex-col items-center justify-center px-6 py-12 gap-3">
      <div class="w-16 h-16 rounded-full bg-iso-bg-elevated border border-iso-border-subtle flex items-center justify-center">
        <Icon name="lucide:boxes" class="w-7 h-7 text-iso-text-muted" />
      </div>
      <h2 class="font-mono text-base text-iso-text-primary">No stacks yet</h2>
      <p class="text-sm text-iso-text-muted max-w-md text-center leading-relaxed">
        Stacks appear automatically when your hosts report containers labelled <code class="font-mono text-xs text-iso-text-secondary">com.docker.compose.project</code> or <code class="font-mono text-xs text-iso-text-secondary">isengard.stack</code>.
      </p>
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
