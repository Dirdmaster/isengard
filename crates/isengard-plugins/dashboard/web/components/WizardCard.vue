<script setup lang="ts">
withDefaults(defineProps<{
  width?: number
  contentGap?: number
  padding?: string
}>(), {
  width: 580,
  contentGap: 18,
  padding: 'p-10',
})
</script>

<template>
  <div
    class="rounded-xl bg-iso-bg-elevated border border-iso-border-subtle flex flex-col"
    :style="{ width: width + 'px' }"
  >
    <div
      :class="['flex flex-col items-center', padding]"
      :style="{ gap: contentGap + 'px' }"
    >
      <slot />
    </div>
    <footer
      v-if="$slots.actions"
      class="flex items-center justify-end gap-3 px-10 py-4 border-t border-iso-border-subtle"
    >
      <slot name="actions" />
    </footer>
  </div>
</template>
