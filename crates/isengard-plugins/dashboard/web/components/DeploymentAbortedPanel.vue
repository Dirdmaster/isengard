<script setup lang="ts">
import { ref } from 'vue'
import type { DeploymentDto } from '~/composables/useDeployments'

interface Props {
  deployment: DeploymentDto
}
const props = defineProps<Props>()
const emit = defineEmits<{ (e: 'dismiss'): void }>()

const retrying = ref(false)
const api = useApi()

function shortDigest(d: string | null | undefined): string {
  if (!d) return '—'
  const colon = d.indexOf(':')
  const body = colon >= 0 ? d.slice(colon + 1) : d
  return body.slice(0, 12)
}

const stateLabel = computed(() => props.deployment.state.replace(/_/g, ' '))

const headerDotClass = computed(() => {
  if (props.deployment.state === 'failed')  return 'bg-iso-error'
  if (props.deployment.state === 'aborted') return 'bg-iso-warn'
  return 'bg-iso-text-faint'
})

const statePill = computed<{ state: 'success' | 'warn' | 'error' | 'info' | 'neutral'; label: string }>(() => {
  switch (props.deployment.state) {
    case 'aborted': return { state: 'warn',  label: 'aborted' }
    case 'failed':  return { state: 'error', label: 'failed' }
    default:        return { state: 'neutral', label: stateLabel.value }
  }
})

async function onRetry() {
  if (retrying.value) return
  retrying.value = true
  try {
    await api.post(`/stacks/${props.deployment.stack_id}/actions/force-update`, {})
    useToast().success('Retry queued — force update requested')
    emit('dismiss')
  } catch (e) {
    useToast().error(`Retry failed: ${e instanceof Error ? e.message : String(e)}`)
    retrying.value = false
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
          Deployment {{ stateLabel }}
        </span>
        <StatusPill :state="statePill.state" :label="statePill.label" size="xs" />
      </div>
      <button
        class="text-xs text-iso-text-muted hover:text-iso-text"
        @click="emit('dismiss')"
      >
        Dismiss
      </button>
    </div>

    <!-- Service + digests -->
    <div class="grid grid-cols-3 gap-4 mb-3 text-xs">
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Service</div>
        <div class="font-mono text-iso-text">{{ deployment.service_name }}</div>
      </div>
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Blue (live)</div>
        <div class="font-mono text-iso-text-muted">{{ shortDigest(deployment.blue_digest) }}</div>
      </div>
      <div>
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">Green (target)</div>
        <div class="font-mono text-iso-text-faint">{{ shortDigest(deployment.green_digest) }}</div>
      </div>
    </div>

    <!-- Reason -->
    <div class="text-xs text-iso-text-muted mb-3">
      <span class="text-iso-text-faint uppercase tracking-wider mr-2">Reason</span>
      {{ deployment.error || 'unknown' }}
    </div>

    <!-- Recovery hint + retry -->
    <div class="flex items-center justify-between pt-3 border-t border-iso-border-subtle/40">
      <span class="text-xs text-iso-text-muted">
        Blue ({{ shortDigest(deployment.blue_digest) }}) is still serving traffic.
      </span>
      <button
        class="text-xs px-3 py-1 rounded border border-iso-info/40 text-iso-info hover:bg-iso-info/10 disabled:opacity-40 disabled:hover:bg-transparent"
        :disabled="retrying"
        @click="onRetry"
      >
        {{ retrying ? 'Retrying...' : 'Retry' }}
      </button>
    </div>
  </section>
</template>
