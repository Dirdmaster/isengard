<template>
  <div class="border-l border-iso-border-subtle bg-iso-bg-elevated p-4 overflow-y-auto" v-if="event">
    <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint">SELECTED EVENT · {{ formatTime(event.occurred_at) }}</div>

    <div class="flex items-center gap-2 mt-2 mb-1">
      <div class="w-2 h-2 rounded-full" :style="{ backgroundColor: kindColor }"></div>
      <h4 class="text-lg font-semibold text-iso-text-primary">{{ event.container_name ?? event.kind }}</h4>
    </div>

    <div class="text-iso-xs text-iso-text-muted font-mono mb-4">{{ event.image ?? event.summary }}</div>

    <div class="space-y-1.5 mb-4">
      <KvRow label="Status" :value="event.kind" :value-class="kindTextClass" />
      <KvRow v-if="event.container_name" label="Container" :value="`/${event.container_name}`" mono />
      <KvRow v-if="event.image" label="Image" :value="event.image" mono />
    </div>

    <div v-if="event.old_digest || event.new_digest" class="mb-4">
      <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">DIGEST CHANGE</div>
      <div v-if="event.old_digest" class="text-iso-xs font-mono p-2 rounded-iso-sm bg-iso-bg-overlay border border-iso-border-subtle text-iso-text-faint">
        was&nbsp;&nbsp;{{ truncDigest(event.old_digest) }}
      </div>
      <div class="text-center text-iso-xs text-iso-text-faint my-1">↓</div>
      <div v-if="event.new_digest" class="text-iso-xs font-mono p-2 rounded-iso-sm bg-iso-bg-overlay border text-iso-success" style="border-color: #1e3826">
        now {{ truncDigest(event.new_digest) }}
      </div>
    </div>

    <hr class="border-iso-border-subtle my-4" />

    <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-2">QUICK ACTIONS</div>
    <div class="space-y-1.5">
      <button class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Open container detail →
      </button>
      <button v-if="event.host_id" class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Open host detail →
      </button>
      <button class="w-full text-left px-3 py-2 rounded-iso-md bg-iso-bg-overlay border border-iso-border-subtle text-iso-sm text-iso-text-secondary hover:border-iso-border-strong">
        Filter timeline to this container
      </button>
    </div>
  </div>
  <div v-else class="border-l border-iso-border-subtle bg-iso-bg-elevated p-6 text-iso-text-faint text-iso-sm">
    Select an event to see details.
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const ui = useUiStore()
const eventsStore = useEventsStore()

const event = computed(() => {
  if (ui.selectedEventId === null) return null
  return eventsStore.events.find((e: any) => e.id === ui.selectedEventId) ?? null
})

const kindColor = computed(() => {
  if (!event.value) return '#94a3b8'
  return kindToColor(event.value.kind)
})

const kindTextClass = computed(() => {
  if (!event.value) return ''
  const k = event.value.kind
  if (k === 'update.success') return 'text-iso-success'
  if (k === 'update.failed') return 'text-iso-error'
  if (k === 'update.pulling') return 'text-iso-warn'
  if (k === 'agent.disconnect_long') return 'text-iso-info'
  return 'text-iso-neutral'
})

function kindToColor(k: string) {
  if (k === 'update.success') return '#4ade80'
  if (k === 'update.failed') return '#f87171'
  if (k === 'update.pulling') return '#fbbf24'
  if (k === 'agent.disconnect_long') return '#c084fc'
  return '#94a3b8'
}

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString()
}

function truncDigest(d: string) {
  if (d.length <= 24) return d
  return d.slice(0, 16) + '…'
}
</script>
