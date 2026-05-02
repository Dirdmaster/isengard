<script setup lang="ts">
defineProps<{
  step: 1 | 2 | 3 | 4
}>()
</script>

<template>
  <div class="fixed inset-0 z-50 bg-iso-bg-base text-iso-text-primary flex items-center justify-center overflow-y-auto">
    <div class="absolute top-6 left-7 flex items-center gap-2.5">
      <span class="w-2.5 h-2.5 rounded-full bg-iso-success"></span>
      <span class="font-semibold text-iso-text-primary tracking-tight text-[15px]">isengard</span>
    </div>

    <span
      v-if="step < 4"
      class="absolute top-7 right-7 text-[11px] font-mono text-iso-text-faint"
    >{{ step }} of 3</span>
    <span
      v-else
      class="absolute top-7 right-7 text-[11px] font-mono text-iso-success"
    >Done</span>

    <slot />
  </div>
</template>
