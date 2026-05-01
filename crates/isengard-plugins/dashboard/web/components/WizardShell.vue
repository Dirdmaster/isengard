<script setup lang="ts">
defineProps<{
  step: 1 | 2 | 3 | 4
}>()
</script>

<template>
  <div class="fixed inset-0 z-50 bg-iso-bg-base text-iso-text-primary flex flex-col">
    <header class="flex items-center justify-between px-7 py-5 shrink-0">
      <div class="flex items-center gap-2.5">
        <span class="w-2.5 h-2.5 rounded-full bg-iso-success"></span>
        <span class="font-semibold text-iso-text-primary tracking-tight text-[15px]">isengard</span>
      </div>
      <span class="text-[11px] font-mono text-iso-text-faint" v-if="step < 4">{{ step }} of 3</span>
      <span class="text-[11px] font-mono text-iso-success" v-else>Done</span>
    </header>

    <main class="flex-1 flex flex-col min-h-0 px-10">
      <slot />
    </main>

    <footer class="border-t border-iso-border-subtle shrink-0 flex items-center justify-between gap-4 min-h-14 px-7 py-3">
      <slot name="footer-left" />
      <div class="flex items-center gap-3">
        <slot name="footer-right" />
      </div>
    </footer>
  </div>
</template>
