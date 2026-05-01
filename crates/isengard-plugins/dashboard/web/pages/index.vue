<template>
  <div class="p-8 max-w-4xl mx-auto">
    <header class="flex items-center gap-3 mb-8">
      <div class="w-3 h-3 rounded-full bg-iso-success"></div>
      <h1 class="text-2xl font-semibold tracking-tight">Isengard Dashboard</h1>
      <span class="text-iso-text-faint text-iso-sm">Phase 5a · scaffold</span>
    </header>

    <section class="space-y-4 mb-12">
      <p class="text-iso-text-secondary">
        Bundle pipeline working. This page is served by the embedded Nuxt 3 SPA.
      </p>
      <p class="text-iso-text-muted text-iso-sm">
        Real UI lands in 5b/5c/5d/5e. This is the foundation.
      </p>
    </section>

    <section>
      <h2 class="text-iso-sm uppercase tracking-wider text-iso-text-faint mb-3">Design tokens</h2>
      <div class="grid grid-cols-6 gap-2">
        <div v-for="t in tokens" :key="t.name" class="rounded-iso-md p-3 border border-iso-border-subtle" :style="{ backgroundColor: t.color }">
          <div class="text-iso-xs font-mono" :class="t.darkText ? 'text-black' : 'text-iso-text-primary'">{{ t.name }}</div>
        </div>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
const tokens = [
  { name: 'success', color: '#4ade80', darkText: true },
  { name: 'warn', color: '#fbbf24', darkText: true },
  { name: 'error', color: '#f87171', darkText: true },
  { name: 'info', color: '#c084fc', darkText: true },
  { name: 'neutral', color: '#94a3b8', darkText: true },
  { name: 'overlay', color: '#15181b', darkText: false },
]
</script>
