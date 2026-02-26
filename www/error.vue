<script setup lang="ts">
const props = defineProps<{ error: { statusCode: number; message: string } }>()

const handleError = () => clearError({ redirect: '/' })
</script>

<template>
  <div class="flex min-h-screen flex-col items-center justify-center bg-bg font-sans text-text px-6">
    <p class="mb-2 text-6xl font-bold text-amber">{{ props.error.statusCode }}</p>
    <p class="mb-8 text-text-muted">{{ props.error.message }}</p>
    <button
      class="rounded-lg bg-amber px-5 py-2.5 text-sm font-medium text-bg transition-all hover:opacity-90"
      @click="handleError"
    >
      Back home
    </button>
  </div>
</template>
