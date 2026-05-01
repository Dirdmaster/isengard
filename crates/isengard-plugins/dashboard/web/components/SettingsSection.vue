<script setup lang="ts">
interface Props {
  title: string
  description?: string
}
defineProps<Props>()
</script>

<template>
  <section class="border-b border-iso-border-subtle py-6">
    <header class="mb-4">
      <h2 class="font-mono text-base">{{ title }}</h2>
      <p v-if="description" class="text-sm text-iso-text-muted mt-1">{{ description }}</p>
    </header>
    <slot />
  </section>
</template>
