<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import pkg from '~/package.json'
import SettingsSection from '~/components/SettingsSection.vue'

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
  { label: 'Version', value: version, mono: true },
  { label: 'Hostname', value: hostname.value, mono: true },
  {
    label: 'Started at',
    value: 'TBD',
    mono: false,
    note: 'Coming with controller info endpoint (Phase 14b/15).',
  },
  {
    label: 'State directory',
    value: '/var/lib/isengard',
    mono: true,
    note: 'Default path. Read from controller once info endpoint lands.',
  },
])
</script>

<template>
  <SettingsSection
    title="Controller identity"
    description="This controller's build and runtime info. Read-only today; some fields are placeholders until the controller info endpoint ships."
  >
    <dl class="grid grid-cols-1 sm:grid-cols-2 gap-4">
      <div
        v-for="f in fields"
        :key="f.label"
        class="rounded-md border border-iso-border-subtle bg-iso-bg-elevated px-4 py-3"
      >
        <dt class="text-[10px] uppercase tracking-wider text-iso-text-faint">{{ f.label }}</dt>
        <dd
          :class="[
            'mt-1 text-sm text-iso-text-primary',
            f.mono ? 'font-mono' : '',
          ]"
        >
          {{ f.value }}
        </dd>
        <p v-if="f.note" class="text-[11px] text-iso-text-muted mt-1">{{ f.note }}</p>
      </div>
    </dl>

    <p class="text-xs text-iso-text-muted mt-4 leading-relaxed">
      Telemetry, updates, defaults, and danger zone are intentionally not shown here yet — each
      needs backend support (settings store, update flow, destructive-action guards) that lands
      in later phases.
    </p>
  </SettingsSection>
</template>
