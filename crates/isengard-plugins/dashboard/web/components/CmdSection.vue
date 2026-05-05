<template>
  <div class="px-5 pt-3 pb-1 text-iso-xs uppercase tracking-[0.12em] font-medium text-iso-text-faint">
    {{ label }}
  </div>
</template>

<script setup lang="ts">
defineProps<{ label: string }>()
</script>
