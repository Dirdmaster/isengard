<script setup lang="ts">
import { PhGithubLogo } from '@phosphor-icons/vue'
</script>

<template>
  <footer class="relative z-10 border-t border-border-subtle">
    <div class="mx-auto flex max-w-5xl items-center justify-between px-6 py-6">
      <div class="flex items-center gap-2.5 text-sm text-text-dim">
        <IsengardLogo size="sm" class="text-amber-dim" />
        <span>Isengard</span>
      </div>
      <a
        href="https://github.com/dirdmaster/isengard"
        target="_blank"
        rel="noopener"
        class="text-text-dim transition-colors hover:text-text-muted"
      >
        <PhGithubLogo :size="16" weight="regular" />
      </a>
    </div>
  </footer>
</template>
