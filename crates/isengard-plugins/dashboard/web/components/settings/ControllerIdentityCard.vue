<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import pkg from '~/package.json'

/**
 * Controller identity card — aligned to `design/concepts/settings-general/v1.html`.
 *
 * Concept renders an elevated card with a small-caps section heading and a
 * 2-column [180px_1fr] grid of label/value rows. Telemetry / Updates /
 * Defaults / Danger zone from the concept are intentionally NOT rebuilt here:
 * each one needs backend support (settings store, update flow, destructive-
 * action guards) that lands in later phases. Building hollow shells would be
 * design dishonesty.
 */

const version = (pkg as { version?: string }).version ?? 'dev'

// `window.location.hostname` is fine for the dashboard origin: that's how
// the operator reached this page. Defaults to `controller.local` per
// ISENGARD_CONTROLLER_DNS in production setups.
const hostname = ref<string>('controller.local')
onMounted(() => {
  if (typeof window !== 'undefined' && window.location?.hostname) {
    hostname.value = window.location.hostname
  }
})

const fields = computed(() => [
  { label: 'Public hostname', value: hostname.value, mono: true },
  { label: 'Version', value: version, mono: true },
  {
    label: 'State directory',
    value: '/var/lib/isengard',
    mono: true,
    note: 'Default path. Read from controller once info endpoint lands.',
  },
  {
    label: 'Started at',
    value: '— pending controller info endpoint',
    mono: false,
  },
])
</script>

<template>
  <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-5 flex flex-col gap-4 mb-6">
    <span class="text-[10px] font-semibold text-iso-text-muted tracking-widest">CONTROLLER IDENTITY</span>

    <div class="grid grid-cols-[180px_1fr] items-center gap-y-3 gap-x-6">
      <template v-for="f in fields" :key="f.label">
        <label class="text-xs text-iso-text-muted">{{ f.label }}</label>
        <div
          class="px-3 py-2 rounded-iso-md bg-iso-bg-base border border-iso-border-subtle text-xs text-iso-text-primary"
          :class="f.mono ? 'font-mono' : ''"
        >
          {{ f.value }}
          <span v-if="f.note" class="text-iso-text-faint ml-2">({{ f.note }})</span>
        </div>
      </template>
    </div>

    <p class="text-[11px] text-iso-text-muted leading-relaxed">
      Telemetry, updates, defaults, and danger zone are intentionally not shown yet — each
      needs backend support (settings store, update flow, destructive-action guards) that lands
      in later phases.
    </p>
  </section>
</template>
