<script setup lang="ts">
import { computed, ref } from 'vue'
import {
  useEffectivePolicy,
  type PolicyOrigin,
  type EffectivePolicyContext,
} from '~/composables/useEffectivePolicy'

interface Props {
  fleet?: string
  stack?: string
  service?: string
  host_id?: string
  container?: string
}

const props = defineProps<Props>()

const ctx = computed<EffectivePolicyContext>(() => ({
  fleet: props.fleet,
  stack: props.stack,
  service: props.service,
  host_id: props.host_id,
  container: props.container,
}))

const { effective, loading, error, load } = useEffectivePolicy(ctx)

const open = ref(false)
const everLoaded = ref(false)

async function toggle() {
  open.value = !open.value
  if (open.value && !everLoaded.value) {
    everLoaded.value = true
    await load()
  }
}

async function retry() {
  await load()
}

/**
 * Provenance label per the spec (T7 brief):
 *
 *   default   -> from DEFAULTS
 *   global    -> from GLOBAL DEFAULT
 *   fleet     -> from FLEET (no name available from this endpoint)
 *   stack     -> from STACK
 *   service   -> from SERVICE
 *   container -> from CONTAINER LABEL
 */
function originLabel(origin: PolicyOrigin): string {
  switch (origin) {
    case 'default': return 'from DEFAULTS'
    case 'global': return 'from GLOBAL DEFAULT'
    case 'fleet': return 'from FLEET'
    case 'stack': return 'from STACK'
    case 'service': return 'from SERVICE'
    case 'container': return 'from CONTAINER LABEL'
  }
}

function formatPausedUntil(value: string | null): string {
  if (!value) return '-'
  // Best-effort human readable; falls back to the raw string if the browser
  // can't parse it.
  const d = new Date(value)
  if (Number.isNaN(d.getTime())) return value
  return d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z')
}

interface Row {
  field: string
  value: string
  origin: PolicyOrigin
  /** When true the value renders as faint dash-style to signal "not set". */
  empty: boolean
  /**
   * Optional accent class applied to the value pill. Mirrors the concept's
   * info / warn coloring on strategy + gate.
   */
  pillClass?: string
}

const rows = computed<Row[]>(() => {
  const r = effective.value
  if (!r) return []

  return [
    {
      field: 'strategy',
      value: r.strategy,
      origin: r.provenance.strategy,
      empty: false,
      pillClass: 'bg-iso-info-soft text-iso-info',
    },
    {
      field: 'gate',
      value: r.gate,
      origin: r.provenance.gate,
      empty: false,
      pillClass: r.gate === 'approval'
        ? 'bg-iso-warn-soft text-iso-warn'
        : r.gate === 'never'
          ? 'bg-iso-error-soft text-iso-error'
          : 'bg-iso-info-soft text-iso-info',
    },
    {
      field: 'paused_until',
      value: r.paused_until ? formatPausedUntil(r.paused_until) : '-',
      origin: r.provenance.paused_until,
      empty: !r.paused_until,
    },
    {
      field: 'on_failure',
      value: r.on_failure,
      origin: r.provenance.on_failure,
      empty: false,
      pillClass: 'bg-iso-info-soft text-iso-info',
    },
    {
      field: 'approver',
      value: r.approver_channel || '-',
      origin: r.provenance.approver_channel,
      empty: !r.approver_channel,
    },
  ]
})
</script>

<template>
  <div class="border-t border-iso-border-subtle">
    <button
      type="button"
      class="w-full px-4 py-2 flex items-center gap-2 text-left hover:bg-iso-bg-overlay/40 transition-colors"
      :aria-expanded="open"
      @click="toggle"
    >
      <svg
        class="w-3 h-3 text-iso-text-muted shrink-0 transition-transform"
        :class="open ? 'rotate-90' : ''"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        viewBox="0 0 24 24"
      >
        <polyline points="9 6 15 12 9 18" />
      </svg>
      <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
        EFFECTIVE POLICY
      </span>
    </button>

    <div v-if="open" class="px-4 pb-3">
      <div v-if="loading" class="text-[11px] text-iso-text-muted py-2">
        Loading...
      </div>

      <div
        v-else-if="error"
        class="rounded-iso-md bg-iso-error-soft border border-iso-error p-2 flex items-center justify-between gap-3"
      >
        <span class="text-[11px] text-iso-error">{{ error }}</span>
        <button
          type="button"
          class="text-[11px] text-iso-info underline"
          @click="retry"
        >
          Retry
        </button>
      </div>

      <div
        v-else-if="effective"
        class="rounded-iso-md bg-iso-bg-base border border-iso-border-subtle overflow-hidden"
      >
        <div
          v-for="(row, idx) in rows"
          :key="row.field"
          class="grid grid-cols-[120px_140px_1fr] px-3 py-2 text-xs items-center"
          :class="idx < rows.length - 1 ? 'border-b border-iso-border-subtle' : ''"
        >
          <div class="font-mono text-[11px] text-iso-text-muted">
            {{ row.field }}
          </div>
          <div>
            <span
              v-if="row.empty"
              class="font-mono text-[11px] text-iso-text-faint"
            >-</span>
            <span
              v-else-if="row.pillClass"
              class="px-1.5 py-px rounded-iso-sm font-mono text-[11px]"
              :class="row.pillClass"
            >{{ row.value }}</span>
            <span
              v-else
              class="font-mono text-[11px] text-iso-text-secondary"
            >{{ row.value }}</span>
          </div>
          <div
            class="text-[11px]"
            :class="row.origin === 'default'
              ? 'text-iso-text-faint'
              : 'text-iso-text-secondary'"
          >
            ({{ originLabel(row.origin) }})
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
