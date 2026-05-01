<template>
  <div class="px-5 pt-2.5 pb-1 text-iso-xs uppercase tracking-wider font-medium text-iso-text-faint">
    {{ label }}
  </div>
</template>

<script setup lang="ts">
defineProps<{ label: string }>()
</script>
