<template>
  <div class="flex-1 flex flex-col items-center justify-center px-6 py-12 gap-3">
    <div class="w-16 h-16 rounded-full bg-iso-bg-elevated border border-iso-border-subtle flex items-center justify-center">
      <Icon :name="`lucide:${icon}`" class="w-7 h-7 text-iso-text-muted" />
    </div>
    <h2 class="font-mono text-base text-iso-text-primary">{{ title }}</h2>
    <p v-if="$slots.default || description" class="text-sm text-iso-text-muted max-w-md text-center leading-relaxed">
      <slot>{{ description }}</slot>
    </p>
    <div v-if="$slots.cta" class="mt-1">
      <slot name="cta" />
    </div>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  icon: string
  title: string
  description?: string
}>()
</script>
