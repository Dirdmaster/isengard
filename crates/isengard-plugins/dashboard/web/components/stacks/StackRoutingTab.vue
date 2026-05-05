<template>
  <EmptyState
    icon="route"
    title="Per-stack routing rules"
    description="Coming with Phase 8.x stack-scoped routing. For now, manage rules globally."
  >
    <template #cta>
      <NuxtLink
        to="/settings?tab=networking&subtab=proxy"
        class="text-xs px-3 py-1.5 rounded-md border border-iso-border-subtle text-iso-info hover:border-iso-info hover:bg-iso-info/10 transition-colors"
      >
        Open global routing rules
      </NuxtLink>
    </template>
  </EmptyState>
</template>

<script setup lang="ts">
import EmptyState from '~/components/EmptyState.vue'
</script>
