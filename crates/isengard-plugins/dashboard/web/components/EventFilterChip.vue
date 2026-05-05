<script setup lang="ts">
import { computed } from 'vue'

type Tone = 'success' | 'warn' | 'error' | 'info' | 'neutral'

interface Props {
  label: string
  active?: boolean
  count?: number
  tone?: Tone
}

const props = withDefaults(defineProps<Props>(), {
  tone: 'neutral',
})

defineEmits<{ toggle: [] }>()

// Active chip = solid soft-bg with tone-colored text. Mirrors the concept v1
// filter bar where the selected `update.*` chip uses iso-info as its bg and
// inverts text to iso-bg-base. We keep a softer variant here for legibility.
const chipClass = computed(() => {
  if (props.active) {
    switch (props.tone) {
      case 'success': return 'bg-iso-success-soft text-iso-success border-iso-success/40'
      case 'warn':    return 'bg-iso-warn-soft text-iso-warn border-iso-warn/40'
      case 'error':   return 'bg-iso-error-soft text-iso-error border-iso-error/40'
      case 'info':    return 'bg-iso-info-soft text-iso-info border-iso-info/40'
      default:        return 'bg-iso-bg-overlay text-iso-text-primary border-iso-border-strong'
    }
  }
  return 'bg-iso-bg-base border-iso-border-subtle text-iso-text-secondary hover:text-iso-text-primary'
})
</script>

<template>
  <button
    class="inline-flex items-center gap-1.5 px-2 py-0.5 rounded-iso-sm border font-mono text-[11px] transition-colors"
    :class="chipClass"
    @click="$emit('toggle')"
  >
    <span>{{ label }}</span>
    <span v-if="count !== undefined" class="text-[10px] opacity-70">{{ count }}</span>
  </button>
</template>
