<script setup lang="ts">
interface Props {
  state: 'success' | 'warn' | 'error' | 'info' | 'neutral'
  label: string
  size?: 'xs' | 'sm'
  icon?: string
}

const props = withDefaults(defineProps<Props>(), {
  size: 'sm',
})

const stateClasses: Record<string, string> = {
  success: 'bg-iso-success/15 text-iso-success border-iso-success/30',
  warn:    'bg-iso-warn/15 text-iso-warn border-iso-warn/30',
  error:   'bg-iso-error/15 text-iso-error border-iso-error/30',
  info:    'bg-iso-info/15 text-iso-info border-iso-info/30',
  neutral: 'bg-iso-bg-elevated text-iso-text-muted border-iso-border-subtle',
}

const sizeClasses: Record<string, string> = {
  xs: 'text-[10px] px-1.5 py-0.5 gap-1',
  sm: 'text-xs px-2 py-0.5 gap-1.5',
}
</script>

<template>
  <span
    class="inline-flex items-center rounded-full border font-medium"
    :class="[stateClasses[state], sizeClasses[size]]"
  >
    <Icon v-if="icon" :name="icon" class="w-3 h-3" />
    {{ label }}
  </span>
</template>
