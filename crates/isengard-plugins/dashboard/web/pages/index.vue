<template>
  <div class="p-8 max-w-4xl mx-auto">
    <header class="flex items-center gap-3 mb-8">
      <div class="w-3 h-3 rounded-full" :class="connected ? 'bg-iso-success' : 'bg-iso-error'"></div>
      <h1 class="text-2xl font-semibold tracking-tight">Isengard Dashboard</h1>
      <span class="text-iso-text-faint text-iso-sm">Phase 5b · API + WS</span>
    </header>

    <section class="grid grid-cols-3 gap-4 mb-8">
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Hosts</div>
        <div class="text-2xl font-mono">{{ hostsStore.hosts.length }}</div>
      </div>
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Events (recent)</div>
        <div class="text-2xl font-mono">{{ eventsStore.events.length }}</div>
      </div>
      <div class="p-4 rounded-iso-md border border-iso-border-subtle bg-iso-bg-elevated">
        <div class="text-iso-xs uppercase tracking-wider text-iso-text-faint mb-1">Live stream</div>
        <div class="text-2xl font-mono" :class="connected ? 'text-iso-success' : 'text-iso-error'">
          {{ connected ? 'connected' : 'offline' }}
        </div>
      </div>
    </section>

    <section v-if="liveEvents.length > 0">
      <h2 class="text-iso-sm uppercase tracking-wider text-iso-text-faint mb-3">Live (last 10)</h2>
      <ul class="space-y-1 font-mono text-iso-xs">
        <li v-for="(e, i) in liveEvents.slice(0, 10)" :key="i" class="text-iso-text-secondary">
          <span class="text-iso-text-faint">{{ new Date(e.occurred_at).toLocaleTimeString() }}</span>
          <span class="ml-2 text-iso-success">{{ e.kind }}</span>
          <span class="ml-2">{{ e.summary }}</span>
        </li>
      </ul>
    </section>
  </div>
</template>

<script setup lang="ts">
const eventsStore = useEventsStore()
const hostsStore = useHostsStore()
const { connected, events: liveEvents } = useEventStream()

await Promise.all([
  eventsStore.load(),
  hostsStore.load(),
])
</script>
