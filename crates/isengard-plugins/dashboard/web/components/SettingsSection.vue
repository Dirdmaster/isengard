<script setup lang="ts">
interface Props {
  title: string
  description?: string
}
defineProps<Props>()
</script>

<template>
  <section class="py-6">
    <header class="mb-4">
      <h2 class="font-mono text-base text-iso-text-primary">{{ title }}</h2>
      <p v-if="description" class="text-sm text-iso-text-muted mt-1">{{ description }}</p>
    </header>
    <slot />
    <Separator class="mt-6 bg-iso-border-subtle" />
  </section>
</template>
