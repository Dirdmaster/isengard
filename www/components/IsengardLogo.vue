<script setup lang="ts">
withDefaults(defineProps<{ size?: 'sm' | 'md' }>(), { size: 'md' })
</script>

<template>
  <svg
    viewBox="0 0 24 32"
    :class="size === 'sm' ? 'h-4' : 'h-7'"
    class="w-auto"
    fill="none"
    stroke="currentColor"
    stroke-width="1.1"
    stroke-linecap="round"
    stroke-linejoin="round"
  >
    <path d="M8 28 L9.5 10 L12 7 L14.5 10 L16 28" />
    <path d="M9.5 10 L7 3" />
    <path d="M11 9 L10 4.5" />
    <path d="M13 9 L14 4.5" />
    <path d="M14.5 10 L17 3" />
    <ellipse
      v-if="size === 'md'"
      cx="12" cy="13.5" rx="1.2" ry="0.6"
      fill="currentColor" opacity="0.8" stroke="none"
    />
  </svg>
</template>
