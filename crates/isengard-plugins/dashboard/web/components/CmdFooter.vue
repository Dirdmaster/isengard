<template>
  <div class="h-9 px-4 border-t border-iso-border-subtle flex items-center justify-between text-iso-xs font-mono text-iso-text-faint">
    <div class="flex items-center gap-3">
      <span class="flex items-center gap-1">
        <kbd class="px-1.5 py-0.5 rounded border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">↑↓</kbd>
        <span>navigate</span>
      </span>
      <span class="flex items-center gap-1">
        <kbd class="px-1.5 py-0.5 rounded border border-iso-border-subtle text-iso-text-muted bg-iso-bg-base">↵</kbd>
        <span>select</span>
      </span>
    </div>
    <div class="flex items-center gap-2">
      <span><span class="text-iso-text-muted">:</span> for actions</span>
      <span class="text-iso-border-strong">·</span>
      <span><span class="text-iso-text-muted">$</span> for shell</span>
      <span class="text-iso-border-strong">·</span>
      <span><span class="text-iso-text-muted">?</span> for help</span>
    </div>
  </div>
</template>

<script setup lang="ts">
</script>
