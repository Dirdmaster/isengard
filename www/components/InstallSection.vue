<script setup lang="ts">
import { PhCopy, PhCheck } from '@phosphor-icons/vue'

const copied = ref(false)

const composeSnippet = `services:
  isengard:
    image: ghcr.io/dirdmaster/isengard
    restart: unless-stopped
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    environment:
      - ISENGARD_INTERVAL=30m
      - ISENGARD_CLEANUP=true`

// Static YAML highlighting. composeSnippet is a hardcoded constant,
// so the v-html usage below is safe from injection.
const composeLines = computed(() =>
  composeSnippet.split('\n').map((line) => {
    const commentMatch = line.match(/^(\s*)(#.*)$/)
    if (commentMatch) {
      return `${commentMatch[1]}<span class="yaml-comment">${commentMatch[2]}</span>`
    }

    const kvMatch = line.match(/^(\s*-?\s*)(\w[\w.]*):(.*)$/)
    if (kvMatch) {
      const [, indent, key, value] = kvMatch
      return `${indent}<span class="yaml-key">${key}</span>:<span class="yaml-value">${value}</span>`
    }

    const listMatch = line.match(/^(\s*- )(.*)$/)
    if (listMatch) {
      return `${listMatch[1]}<span class="yaml-value">${listMatch[2]}</span>`
    }

    return `<span class="yaml-value">${line}</span>`
  }),
)

const copyToClipboard = async () => {
  try {
    await navigator.clipboard.writeText(composeSnippet)
    copied.value = true
    setTimeout(() => (copied.value = false), 2000)
  } catch { /* clipboard unavailable */ }
}

const envVars = [
  { name: 'ISENGARD_INTERVAL', default: '30m', description: 'Check interval (Go duration)' },
  { name: 'ISENGARD_WATCH_ALL', default: 'true', description: 'Watch all containers; set false for opt-in mode' },
  { name: 'ISENGARD_RUN_ONCE', default: 'false', description: 'Run a single cycle, then exit' },
  { name: 'ISENGARD_CLEANUP', default: 'true', description: 'Remove old images after update' },
  { name: 'ISENGARD_STOP_TIMEOUT', default: '30', description: 'Seconds to wait for graceful stop' },
  { name: 'ISENGARD_LOG_LEVEL', default: 'info', description: 'debug, info, warn, error' },
  { name: 'ISENGARD_SELF_UPDATE', default: 'false', description: 'Allow Isengard to update itself' },
]
</script>

<template>
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
      <!-- Code header -->
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
              v-for="v in envVars"
              :key="v.name"
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
</template>
