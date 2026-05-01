<script setup lang="ts">
withDefaults(defineProps<{ rows?: number; columns?: number[] }>(), {
  rows: 6,
  columns: () => [170, 70, 130, 80, 400, 90, 60],
})
</script>

<template>
  <div>
    <div
      v-for="r in rows"
      :key="r"
      class="grid items-center gap-3 px-3 py-3 border-b border-iso-border-subtle animate-pulse"
      :style="{ gridTemplateColumns: columns.map(c => `${c}px`).join(' ') }"
    >
      <div
        v-for="(c, i) in columns"
        :key="i"
        class="h-3 rounded bg-iso-bg-elevated"
        :style="{ width: `${Math.max(40, c - 30)}px` }"
      />
    </div>
  </div>
</template>
