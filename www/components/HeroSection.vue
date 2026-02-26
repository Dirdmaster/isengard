<script setup lang="ts">
import { PhCopy, PhCheck, PhGithubLogo, PhArrowDown } from '@phosphor-icons/vue'

const installCopied = ref(false)
const installCommand = 'docker run -d -v /var/run/docker.sock:/var/run/docker.sock ghcr.io/dirdmaster/isengard'

const copyInstallCommand = async () => {
  try {
    await navigator.clipboard.writeText(installCommand)
    installCopied.value = true
    setTimeout(() => (installCopied.value = false), 2000)
  } catch { /* clipboard unavailable */ }
}
</script>

<template>
  <section class="relative z-10 mx-auto max-w-5xl px-6 pt-16 pb-12 sm:pt-36 sm:pb-28">
    <div class="flex flex-col items-center text-center">
      <!-- Eyebrow -->
      <div
        class="mb-6 flex items-center gap-2 animate-fade-up"
        style="animation-delay: 0.1s"
      >
        <div class="h-px w-8 bg-amber/40" />
        <span class="text-xs tracking-[0.2em] font-medium uppercase text-amber-dim">
          Container auto-updater
        </span>
        <div class="h-px w-8 bg-amber/40" />
      </div>

      <!-- Title -->
      <h1
        class="mb-6 text-5xl font-bold tracking-tight text-amber sm:text-8xl animate-fade-up"
        style="animation-delay: 0.2s"
      >
        Isengard
      </h1>

      <!-- Subtitle -->
      <p
        class="mb-4 text-lg leading-relaxed text-text-muted sm:text-xl animate-fade-up"
        style="animation-delay: 0.3s"
      >
        The tower that never sleeps.
      </p>
      <p
        class="mb-10 max-w-xl text-base leading-relaxed text-text-dim animate-fade-up"
        style="animation-delay: 0.4s"
      >
        A lightweight, zero-config Docker container auto-updater. Detects newer
        images through registry digest checks, then recreates your containers
        with every port, volume, network, and label intact.
      </p>

      <!-- CTAs -->
      <div
        class="mb-0 sm:mb-12 flex flex-wrap items-center justify-center gap-4 animate-fade-up"
        style="animation-delay: 0.5s"
      >
        <NuxtLink
          to="#features"
          class="inline-flex items-center gap-2 rounded-lg bg-amber px-5 py-2.5 text-sm font-medium text-bg transition-all hover:opacity-90"
        >
          Stay updated
          <PhArrowDown :size="14" weight="bold" />
        </NuxtLink>
        <a
          href="https://github.com/dirdmaster/isengard"
          target="_blank"
          rel="noopener"
          class="inline-flex items-center gap-2 rounded-lg border border-border px-5 py-2.5 text-sm font-medium text-text-muted transition-colors hover:border-amber/30 hover:text-text"
        >
          <PhGithubLogo :size="14" weight="regular" />
          View source
        </a>
      </div>

      <!-- Install bar -->
      <div
        class="hidden sm:block animate-fade-up"
        style="animation-delay: 0.6s"
      >
        <button
          class="group flex items-center gap-5 rounded-lg border border-border-subtle/60 bg-bg-raised/30 px-5 py-3 text-left transition-colors hover:border-border-subtle hover:bg-bg-raised/50"
          @click="copyInstallCommand"
        >
          <code class="whitespace-nowrap text-[13px] font-mono"><!--
            --><span class="text-text-dim/60">$</span> <span class="text-amber-dim">docker run</span> <span class="text-text-dim">-d</span> <span class="text-text-dim">-v</span> <span class="text-text-muted">/var/run/docker.sock</span> <span class="text-amber">ghcr.io/dirdmaster/isengard</span><!--
          --></code>
          <span class="flex shrink-0 items-center text-text-dim/40 transition-colors group-hover:text-text-dim">
            <PhCheck v-if="installCopied" :size="14" weight="bold" class="text-amber" />
            <PhCopy v-else :size="14" weight="regular" />
          </span>
        </button>
      </div>
    </div>
  </section>
</template>
