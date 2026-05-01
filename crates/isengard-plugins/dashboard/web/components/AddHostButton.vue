<script setup lang="ts">
defineEmits<{ click: [] }>()
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-3 py-1.5 rounded border border-iso-border-subtle hover:border-iso-success hover:text-iso-success text-sm transition-colors"
    @click="$emit('click')"
  >
    <Icon name="lucide:plus" class="w-3.5 h-3.5" />
    Add host
  </button>
</template>
