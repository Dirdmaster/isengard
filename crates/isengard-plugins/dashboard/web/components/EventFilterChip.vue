<script setup lang="ts">
interface Props {
  label: string
  active?: boolean
  count?: number
}

defineProps<Props>()
defineEmits<{ toggle: [] }>()
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-xs border transition-colors"
    :class="active
      ? 'border-iso-success bg-iso-success/10 text-iso-success'
      : 'border-iso-border-subtle text-iso-text-muted hover:text-iso-text-primary'"
    @click="$emit('toggle')"
  >
    {{ label }}
    <span v-if="count !== undefined" class="font-mono text-[10px] opacity-70">{{ count }}</span>
  </button>
</template>
