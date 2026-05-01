<script setup lang="ts">
import { useToastsStore } from '~/stores/toasts'

const store = useToastsStore()
</script>

<template>
  <div class="fixed bottom-12 right-4 z-50 flex flex-col gap-2 pointer-events-auto">
    <Toast
      v-for="t in store.items"
      :key="t.id"
      :toast="t"
    />
  </div>
</template>
