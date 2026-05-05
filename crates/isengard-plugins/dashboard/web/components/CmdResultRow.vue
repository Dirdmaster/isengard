<template>
  <button
    class="w-full flex items-center gap-3 pl-5 pr-4 py-2 transition-colors text-left relative border-l-2"
    :class="highlighted
      ? 'bg-iso-bg-row-hover border-iso-info'
      : 'border-transparent hover:bg-iso-bg-row-hover'"
    @click="$emit('select')"
    @mouseenter="$emit('hover')"
  >
    <Icon
      :name="icon"
      class="w-4 h-4 shrink-0"
      :class="iconClass ?? (highlighted ? 'text-iso-text-secondary' : 'text-iso-text-muted')"
    />
    <span
      class="text-iso-base font-semibold shrink-0"
      :class="highlighted ? 'text-iso-text-primary' : 'text-iso-text-secondary'"
    >{{ label }}</span>
    <span
      v-if="meta"
      class="text-iso-sm text-iso-text-muted truncate min-w-0"
    >{{ meta }}</span>
    <div class="flex-1"></div>
    <span
      v-if="hint"
      class="flex items-center gap-1 text-iso-xs font-mono text-iso-text-faint shrink-0"
    >
      <Icon :name="hintIcon ?? 'lucide:corner-down-left'" class="w-3 h-3" />
      <span>{{ hint }}</span>
    </span>
  </button>
</template>

<script setup lang="ts">
defineProps<{
  icon: string
  iconClass?: string
  label: string
  meta?: string
  hint?: string
  hintIcon?: string
  highlighted?: boolean
}>()
defineEmits<{ select: []; hover: [] }>()
</script>
