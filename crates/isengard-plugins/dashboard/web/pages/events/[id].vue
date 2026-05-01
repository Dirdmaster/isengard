<script setup lang="ts">
import { computed } from 'vue'
import { useEventsStore } from '~/stores/events'

const route = useRoute()
const id = computed(() => Number(route.params.id))

const eventsStore = useEventsStore()
await eventsStore.fetchOne(id.value)

const event = computed(() => eventsStore.events.find((e) => e.id === id.value))
</script>

<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />
    <div v-if="event" class="p-6 max-w-3xl">
      <NuxtLink to="/events" class="text-xs text-iso-text-muted hover:text-iso-text-primary">
        ← Events
      </NuxtLink>
      <h1 class="font-mono text-lg mt-1">
        <span class="text-iso-success">{{ event.kind }}</span>
        {{ event.summary }}
      </h1>
      <div class="text-sm text-iso-text-muted mt-1">
        {{ event.occurred_at }}
        <span v-if="event.received_at"> · received {{ event.received_at }}</span>
      </div>

      <section v-if="event.metadata" class="mt-6">
        <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-2">Metadata</h2>
        <pre class="text-xs font-mono bg-iso-bg-elevated rounded p-3 overflow-x-auto">{{ JSON.stringify(event.metadata, null, 2) }}</pre>
      </section>
    </div>
    <div v-else class="p-6 text-iso-text-muted">Event not found.</div>
  </div>
</template>
