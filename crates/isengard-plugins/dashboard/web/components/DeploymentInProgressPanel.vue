<script setup lang="ts">
import { computed } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

interface Props {
  deployment: DeploymentDto
}
const props = defineProps<Props>()

// ---- State machine -------------------------------------------------------

/** Linear "happy path" states the driver walks through. */
const STEPS: Array<{ key: string; label: string }> = [
  { key: 'pending',         label: 'pending' },
  { key: 'spinning_up',     label: 'spinning up' },
  { key: 'switching',       label: 'switching' },
  { key: 'draining',        label: 'draining' },
  { key: 'destroying_blue', label: 'destroying blue' },
]

const TERMINAL_STATES = new Set(['done', 'aborted', 'failed'])

const isTerminal = computed(() => TERMINAL_STATES.has(props.deployment.state))
const isRecovering = computed(() => props.deployment.state === 'aborted' && !!props.deployment.error)

const currentIndex = computed(() => {
  const idx = STEPS.findIndex((s) => s.key === props.deployment.state)
  if (idx >= 0) return idx
  // Terminal state: treat as past the last step.
  if (props.deployment.state === 'done') return STEPS.length
  return -1
})

function stepStatus(i: number): 'done' | 'active' | 'pending' {
  const cur = currentIndex.value
  if (cur < 0) return 'pending'
  if (i < cur) return 'done'
  if (i === cur) return 'active'
  return 'pending'
}

// ---- Pill + dot styling --------------------------------------------------

const stateLabel = computed(() => props.deployment.state.replace(/_/g, ' '))

const statePill = computed<{ state: 'success' | 'warn' | 'error' | 'info' | 'neutral'; label: string }>(() => {
  switch (props.deployment.state) {
    case 'done':     return { state: 'success', label: 'done' }
    case 'failed':   return { state: 'error',   label: 'failed' }
    case 'aborted':  return { state: 'warn',    label: 'aborted' }
    case 'pending':  return { state: 'neutral', label: 'pending' }
    default:         return { state: 'info',    label: stateLabel.value }
  }
})

const headerDotClass = computed(() => {
  if (props.deployment.state === 'failed')  return 'bg-iso-error'
  if (props.deployment.state === 'aborted') return 'bg-iso-warn'
  if (props.deployment.state === 'done')    return 'bg-iso-success'
  return 'bg-iso-info animate-pulse'
})

// ---- Digest formatting ---------------------------------------------------

function shortDigest(d: string | null | undefined): string {
  if (!d) return '—'
  // sha256:abcd... → abcd1234 (first 12 of the digest body).
  const colon = d.indexOf(':')
  const body = colon >= 0 ? d.slice(colon + 1) : d
  return body.slice(0, 12)
}

// ---- Abort ---------------------------------------------------------------

const aborting = ref(false)

async function abort() {
  if (isTerminal.value || aborting.value) return
  aborting.value = true
  try {
    const api = useApi()
    await api.post(`/deployments/${props.deployment.id}/abort`, {})
    useToast().success('Abort requested')
  } catch (e) {
    // Task 10 lands the endpoint; until then a 404 is expected.
    useToast().error(`Abort failed: ${e instanceof Error ? e.message : String(e)}`)
  } finally {
    aborting.value = false
  }
}
</script>

<template>
  <section class="border border-iso-border-subtle rounded-md bg-iso-bg-elevated p-4 mb-6">
    <!-- Header -->
    <div class="flex items-center justify-between mb-3">
      <div class="flex items-center gap-2">
        <span class="w-2 h-2 rounded-full" :class="headerDotClass"></span>
        <span class="text-sm font-medium text-iso-text">
          Deployment in progress
        </span>
        <StatusPill :state="statePill.state" :label="statePill.label" size="xs" />
      </div>
      <button
        class="text-xs px-2 py-1 rounded border border-iso-border-subtle text-iso-text-muted hover:text-iso-error hover:border-iso-error/40 disabled:opacity-40 disabled:hover:text-iso-text-muted disabled:hover:border-iso-border-subtle"
        :disabled="isTerminal || aborting"
        @click="abort"
      >
        {{ aborting ? 'aborting...' : 'Abort' }}
      </button>
    </div>

    <!-- Service + digest -->
    <div class="grid grid-cols-3 gap-4 mb-4 text-xs">
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Service</div>
        <div class="font-mono text-iso-text">{{ deployment.service_name }}</div>
      </div>
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Blue</div>
        <div class="font-mono text-iso-text-muted">{{ shortDigest(deployment.blue_digest) }}</div>
      </div>
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Green</div>
        <div class="font-mono text-iso-text">{{ shortDigest(deployment.green_digest) }}</div>
      </div>
    </div>

    <!-- Step list -->
    <ol class="space-y-1">
      <li
        v-for="(step, i) in STEPS"
        :key="step.key"
        class="flex items-center gap-2 text-xs"
      >
        <span class="w-3 text-center" :class="{
          'text-iso-success': stepStatus(i) === 'done',
          'text-iso-info':    stepStatus(i) === 'active',
          'text-iso-text-faint': stepStatus(i) === 'pending',
        }">▸</span>
        <span :class="{
          'text-iso-success': stepStatus(i) === 'done',
          'text-iso-text font-medium': stepStatus(i) === 'active',
          'text-iso-text-faint': stepStatus(i) === 'pending',
        }">{{ step.label }}</span>
        <span
          v-if="stepStatus(i) === 'active' && !isTerminal"
          class="text-iso-text-faint italic"
        >
          in progress
        </span>
      </li>

      <!-- Optional recovering row when aborted with an error message. -->
      <li v-if="isRecovering" class="flex items-start gap-2 text-xs pt-1 border-t border-iso-border-subtle/40 mt-2">
        <span class="w-3 text-center text-iso-warn">▸</span>
        <span class="text-iso-warn font-medium">Recovering</span>
        <span class="text-iso-text-muted">{{ deployment.error }}</span>
      </li>
    </ol>
  </section>
</template>
