<template>
  <EmptyState
    icon="file-code"
    title="Compose source view"
    description="Coming when stack source is stored alongside the stack. Today, compose YAML lives on the host's filesystem and isn't synced back to the controller."
  />
</template>

<script setup lang="ts">
import EmptyState from '~/components/EmptyState.vue'
</script>
