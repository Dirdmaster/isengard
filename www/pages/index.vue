<script setup lang="ts">
import {
  PhEye,
  PhLightning,
  PhShieldCheck,
  PhFeather,
  PhCopy,
  PhCheck,
  PhGithubLogo,
  PhArrowDown,
} from '@phosphor-icons/vue'

const copied = ref(false)
const installCopied = ref(false)
const scrolled = ref(false)

onMounted(() => {
  const onScroll = () => { scrolled.value = window.scrollY > 20 }
  window.addEventListener('scroll', onScroll, { passive: true })
  onUnmounted(() => window.removeEventListener('scroll', onScroll))
})

const installCommand = 'docker run -d -v /var/run/docker.sock:/var/run/docker.sock ghcr.io/dirdmaster/isengard'

function copyInstallCommand() {
  navigator.clipboard.writeText(installCommand)
  installCopied.value = true
  setTimeout(() => (installCopied.value = false), 2000)
}

const composeSnippet = `services:
  isengard:
    image: ghcr.io/dirdmaster/isengard
    restart: unless-stopped
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - ~/.docker/config.json:/root/.docker/config.json:ro
    environment:
      - ISENGARD_INTERVAL=30m
      - ISENGARD_CLEANUP=true`

const composeLines = computed(() => {
  return composeSnippet.split('\n').map((line) => {
    // YAML key: value highlighting
    const commentMatch = line.match(/^(\s*)(#.*)$/)
    if (commentMatch) {
      return `${commentMatch[1]}<span class="yaml-comment">${commentMatch[2]}</span>`
    }

    const kvMatch = line.match(/^(\s*-?\s*)(\w[\w.]*):(.*)$/)
    if (kvMatch) {
      const indent = kvMatch[1]
      const key = kvMatch[2]
      const value = kvMatch[3]
      return `${indent}<span class="yaml-key">${key}</span>:<span class="yaml-value">${value}</span>`
    }

    // List items with paths/values
    const listMatch = line.match(/^(\s*- )(.*)$/)
    if (listMatch) {
      return `${listMatch[1]}<span class="yaml-value">${listMatch[2]}</span>`
    }

    return `<span class="yaml-value">${line}</span>`
  })
})

function copyToClipboard() {
  navigator.clipboard.writeText(composeSnippet)
  copied.value = true
  setTimeout(() => (copied.value = false), 2000)
}

const features = [
  {
    icon: PhEye,
    title: 'Registry-first detection',
    description:
      'Checks remote digests via registry HEAD requests in ~50ms. Images are only pulled when an update is actually found.',
  },
  {
    icon: PhLightning,
    title: 'Zero configuration',
    description:
      'Mount the Docker socket and set an interval. Every running container is watched by default. Opt out with a single label.',
  },
  {
    icon: PhShieldCheck,
    title: 'Faithful recreation',
    description:
      'Ports, volumes, networks, env vars, labels, restart policies, resource limits. Every detail preserved across updates.',
  },
  {
    icon: PhFeather,
    title: 'Vanishingly small',
    description:
      '~3MB scratch-based image. Static Go binary, no runtime dependencies. Uses fewer resources than the containers it guards.',
  },
]

const envVars = [
  { name: 'ISENGARD_INTERVAL', default: '30m', description: 'Check interval (Go duration)' },
  { name: 'ISENGARD_WATCH_ALL', default: 'true', description: 'Watch all containers; set false for opt-in mode' },
  { name: 'ISENGARD_RUN_ONCE', default: 'false', description: 'Run a single cycle, then exit' },
  { name: 'ISENGARD_CLEANUP', default: 'true', description: 'Remove old images after update' },
  { name: 'ISENGARD_STOP_TIMEOUT', default: '30', description: 'Seconds to wait for graceful stop' },
  { name: 'ISENGARD_LOG_LEVEL', default: 'info', description: 'debug, info, warn, error' },
]
</script>

<template>
  <div class="min-h-screen bg-bg font-sans text-text overflow-x-hidden pt-16">
    <!-- Subtle atmospheric background — no gradients, just a soft warm vignette via box-shadow -->
    <div
      class="pointer-events-none fixed inset-0"
      aria-hidden="true"
      style="box-shadow: inset 0 -200px 400px -200px oklch(0.78 0.08 75 / 0.04), inset 0 200px 400px -100px oklch(0.10 0.005 60 / 0.8)"
    />

    <!-- Nav -->
    <nav
      class="fixed top-0 left-0 right-0 z-50 w-full animate-fade-in backdrop-blur-md bg-bg/80 border-b transition-colors"
      :class="scrolled ? 'border-border-subtle/50' : 'border-transparent'"
    >
      <div class="mx-auto flex max-w-5xl items-center justify-between px-6 py-4">
        <a href="#" class="flex items-center gap-2.5 transition-colors hover:opacity-80">
          <!-- Tower logo mark — Orthanc with 4 peaks -->
          <svg
            viewBox="0 0 24 32"
            class="h-7 w-auto text-amber"
            fill="none"
            stroke="currentColor"
            stroke-width="1.1"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M8 28 L9.5 10 L12 7 L14.5 10 L16 28" />
            <path d="M9.5 10 L7 3" />
            <path d="M11 9 L10 4.5" />
            <path d="M13 9 L14 4.5" />
            <path d="M14.5 10 L17 3" />
            <ellipse cx="12" cy="13.5" rx="1.2" ry="0.6" fill="currentColor" opacity="0.8" stroke="none" />
          </svg>
          <span class="text-sm font-medium tracking-wide text-text-muted">Isengard</span>
        </a>
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

    <!-- Hero — centered with install bar -->
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
          <a
            href="#features"
            class="inline-flex items-center gap-2 rounded-lg bg-amber px-5 py-2.5 text-sm font-medium text-bg transition-all hover:opacity-90"
          >
            Stay updated
            <PhArrowDown :size="14" weight="bold" />
          </a>
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

        <!-- Install bar — hidden on mobile, compose snippet covers it -->
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

    <!-- Divider -->
    <div class="mx-auto max-w-5xl px-6">
      <div class="h-px bg-border-subtle" />
    </div>

    <!-- Features -->
    <section id="features" class="relative z-10 mx-auto max-w-5xl scroll-mt-20 px-6 py-16 sm:py-32">
      <div class="grid gap-4 sm:grid-cols-2">
        <div
          v-for="(feature, i) in features"
          :key="i"
          class="group cursor-pointer rounded-xl border border-border-subtle bg-bg-raised/50 p-6 transition-all duration-300 hover:border-border hover:bg-bg-raised animate-fade-up"
          :style="{ animationDelay: `${0.7 + i * 0.1}s` }"
        >
          <!--  -->
          <div
            class="mb-4 flex h-10 w-10 items-center justify-center rounded-lg bg-amber-glow text-amber transition-colors duration-300 group-hover:bg-amber-glow-strong"
          >
            <component :is="feature.icon" :size="20" weight="regular" />
          </div>
          <h3 class="mb-2 text-sm font-semibold tracking-wide">
            {{ feature.title }}
          </h3>
          <p class="text-sm leading-relaxed text-text-muted">
            {{ feature.description }}
          </p>
        </div>
      </div>
    </section>

    <!-- Divider -->
    <div class="mx-auto max-w-5xl px-6">
      <div class="h-px bg-border-subtle" />
    </div>

    <!-- Install -->
    <section
      id="install"
      class="relative z-10 mx-auto max-w-5xl scroll-mt-16 px-6 py-16 sm:py-32"
    >
      <div class="max-w-2xl">
        <div class="mb-6 flex items-center gap-2">
          <div class="h-px w-8 bg-amber/40" />
          <span class="text-xs tracking-[0.2em] font-medium uppercase text-amber-dim">
            Deploy
          </span>
        </div>

        <h2 class="mb-3 text-2xl font-bold tracking-tight sm:text-3xl">
          Up in seconds
        </h2>
        <p class="mb-8 text-base text-text-muted">
          Add Isengard to any existing Compose file, or run it standalone. All it needs is the Docker socket.
        </p>
      </div>

      <!-- Code block -->
      <div class="overflow-hidden rounded-xl border border-border-subtle bg-bg-raised/80">
        <!-- Code header — filename only -->
        <div class="border-b border-border-subtle px-4 py-2.5">
          <span class="text-xs font-medium text-text-dim font-mono">docker-compose.yml</span>
        </div>
        <!-- Code body with syntax highlighting -->
        <!-- eslint-disable vue/no-v-html -->
        <div class="relative overflow-x-auto p-4 sm:p-5">
          <button
            class="absolute top-3 right-3 z-10 hidden sm:flex items-center gap-1.5 rounded-md bg-bg-raised px-2.5 py-1 text-xs text-text-dim transition-colors hover:bg-bg-surface hover:text-text-muted"
            @click="copyToClipboard"
          >
            <PhCheck v-if="copied" :size="13" weight="bold" class="text-amber" />
            <PhCopy v-else :size="13" weight="regular" />
            {{ copied ? 'Copied' : 'Copy' }}
          </button>
          <pre class="text-[10.5px] sm:text-[13px] leading-relaxed font-mono"><code><span v-for="(line, i) in composeLines" :key="i"><span v-html="line" />
</span></code></pre>
        </div>
      </div>

      <!-- Env var table -->
      <div class="mt-10">
        <h3 class="mb-4 text-sm font-semibold tracking-wide">Configuration</h3>
        <div class="overflow-hidden rounded-xl border border-border-subtle">
          <table class="w-full text-sm">
            <thead>
              <tr class="border-b border-border-subtle bg-bg-raised/60">
                <th class="px-4 py-2.5 text-left text-xs font-medium text-text-dim">Variable</th>
                <th class="px-4 py-2.5 text-left text-xs font-medium text-text-dim">Default</th>
                <th class="px-4 py-2.5 text-left text-xs font-medium text-text-dim hidden sm:table-cell">Description</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(v, i) in envVars"
                :key="i"
                class="border-b border-border-subtle last:border-0 transition-colors hover:bg-bg-raised/30"
              >
                <td class="px-4 py-2.5">
                  <code class="text-xs font-mono text-amber-dim">{{ v.name }}</code>
                </td>
                <td class="px-4 py-2.5">
                  <code class="text-xs font-mono text-text-muted">{{ v.default }}</code>
                </td>
                <td class="px-4 py-2.5 text-text-muted hidden sm:table-cell">{{ v.description }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>

      <!-- Opt-out note -->
      <div class="mt-8 rounded-lg border border-border-subtle bg-bg-raised/40 px-5 py-4">
        <div class="space-y-2 text-sm text-text-muted">
          <p>
            <span class="font-medium text-text">Opt-out mode</span> (default):
            all containers are watched. Label individual containers with
            <code class="mx-1 rounded bg-bg-surface px-1.5 py-0.5 text-xs font-mono text-amber-dim">isengard.enable=false</code>
            to exclude them.
          </p>
          <p>
            <span class="font-medium text-text">Opt-in mode</span>:
            set
            <code class="mx-1 rounded bg-bg-surface px-1.5 py-0.5 text-xs font-mono text-amber-dim">ISENGARD_WATCH_ALL=false</code>
            and label only the containers you want watched with
            <code class="mx-1 rounded bg-bg-surface px-1.5 py-0.5 text-xs font-mono text-amber-dim">isengard.enable=true</code>.
          </p>
        </div>
      </div>
    </section>

    <!-- Footer -->
    <footer class="relative z-10 border-t border-border-subtle">
      <div class="mx-auto flex max-w-5xl items-center justify-between px-6 py-6">
        <div class="flex items-center gap-2.5 text-sm text-text-dim">
          <svg
            viewBox="0 0 24 32"
            class="h-4 w-auto text-amber-dim"
            fill="none"
            stroke="currentColor"
            stroke-width="1.1"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M8 28 L9.5 10 L12 7 L14.5 10 L16 28" />
            <path d="M9.5 10 L7 3" />
            <path d="M11 9 L10 4.5" />
            <path d="M13 9 L14 4.5" />
            <path d="M14.5 10 L17 3" />
          </svg>
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
  </div>
</template>
