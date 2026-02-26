<script setup lang="ts">
import { PhGithubLogo } from '@phosphor-icons/vue'

const scrolled = ref(false)

let onScroll: (() => void) | undefined

onMounted(() => {
  onScroll = () => { scrolled.value = window.scrollY > 20 }
  window.addEventListener('scroll', onScroll, { passive: true })
})

onUnmounted(() => {
  if (onScroll) window.removeEventListener('scroll', onScroll)
})
</script>

<template>
  <nav
    class="fixed top-0 left-0 right-0 z-50 w-full animate-fade-in backdrop-blur-md bg-bg/80 border-b transition-colors"
    :class="scrolled ? 'border-border-subtle/50' : 'border-transparent'"
  >
    <div class="mx-auto flex max-w-5xl items-center justify-between px-6 py-4">
      <NuxtLink to="#" class="flex items-center gap-2.5 transition-colors hover:opacity-80">
        <IsengardLogo class="text-amber" />
        <span class="text-sm font-medium tracking-wide text-text-muted">Isengard</span>
      </NuxtLink>
      <a
        href="https://github.com/dirdmaster/isengard"
        target="_blank"
        rel="noopener"
        class="flex items-center gap-2 rounded-md px-3 py-1.5 text-sm text-text-muted transition-colors hover:bg-bg-raised hover:text-text"
      >
        <PhGithubLogo :size="16" weight="regular" />
        <span class="hidden sm:inline">GitHub</span>
      </a>
    </div>
  </nav>
</template>
