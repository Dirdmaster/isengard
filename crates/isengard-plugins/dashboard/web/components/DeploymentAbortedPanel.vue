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
  if (props.deployment.state === 'failed')          return 'bg-iso-error'
  if (props.deployment.state === 'rollback_failed') return 'bg-iso-error'
  if (props.deployment.state === 'aborted')         return 'bg-iso-warn'
  if (props.deployment.state === 'rolled_back')     return 'bg-iso-success'
  return 'bg-iso-text-faint'
})

const statePill = computed<{ state: 'success' | 'warn' | 'error' | 'info' | 'neutral'; label: string }>(() => {
  switch (props.deployment.state) {
    case 'aborted':         return { state: 'warn',    label: 'aborted' }
    case 'failed':          return { state: 'error',   label: 'failed' }
    // Phase 9F (#48):
    case 'rolled_back':     return { state: 'success', label: 'rolled back' }
    case 'rollback_failed': return { state: 'error',   label: 'rollback failed' }
    default:                return { state: 'neutral', label: stateLabel.value }
  }
})

// Phase 9F: route the body off the state. RolledBack is a recovery
// success (no Retry button). RollbackFailed is a hard failure with a
// distinct Retry copy. Anything else falls through to the existing
// abort UX.
const isRolledBack = computed(() => props.deployment.state === 'rolled_back')
const isRollbackFailed = computed(() => props.deployment.state === 'rollback_failed')

function previousDigestShort(): string {
  return shortDigest(props.deployment.previous_digest)
}

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
  <section class="border rounded-md bg-iso-bg-elevated p-4 mb-6"
           :class="{
             'border-iso-success/40': isRolledBack,
             'border-iso-error/40':   isRollbackFailed,
             'border-iso-border-subtle': !isRolledBack && !isRollbackFailed,
           }">
    <!-- Header -->
    <div class="flex items-center justify-between mb-3">
      <div class="flex items-center gap-2">
        <span class="w-2 h-2 rounded-full" :class="headerDotClass"></span>
        <span class="text-sm font-medium text-iso-text">
          Deployment {{ stateLabel }}
        </span>
        <StatusPill :state="statePill.state" :label="statePill.label" size="xs" />
        <!-- Phase 9F (#48) badges -->
        <span
          v-if="isRolledBack"
          class="text-[10px] uppercase tracking-wider px-2 py-0.5 rounded-full bg-iso-success/10 text-iso-success border border-iso-success/30"
        >
          Rolled back to previous digest
        </span>
        <span
          v-if="isRollbackFailed"
          class="text-[10px] uppercase tracking-wider px-2 py-0.5 rounded-full bg-iso-error/10 text-iso-error border border-iso-error/30"
        >
          Rollback failed
        </span>
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
        <div class="text-iso-text-faint uppercase tracking-wider mb-1">
          {{ isRolledBack ? 'Restored' : 'Blue (live)' }}
        </div>
        <div class="font-mono text-iso-text-muted">
          {{ isRolledBack ? previousDigestShort() : shortDigest(deployment.blue_digest) }}
        </div>
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

    <!-- Footer -->
    <div class="flex items-center justify-between pt-3 border-t border-iso-border-subtle/40">
      <!-- Phase 9F: copy varies by terminal state -->
      <span v-if="isRolledBack" class="text-xs text-iso-text-muted">
        Reverted to previous digest ({{ previousDigestShort() }}).
      </span>
      <span v-else-if="isRollbackFailed" class="text-xs text-iso-text-muted">
        Rollback could not complete. Manual intervention may be required.
      </span>
      <span v-else class="text-xs text-iso-text-muted">
        Blue ({{ shortDigest(deployment.blue_digest) }}) is still serving traffic.
      </span>

      <!-- Retry: hidden on RolledBack (nothing to retry); shown on
           RollbackFailed and the legacy aborted/failed paths. -->
      <button
        v-if="!isRolledBack"
        class="text-xs px-3 py-1 rounded border disabled:opacity-40 disabled:hover:bg-transparent"
        :class="isRollbackFailed
          ? 'border-iso-error/40 text-iso-error hover:bg-iso-error/10'
          : 'border-iso-info/40 text-iso-info hover:bg-iso-info/10'"
        :disabled="retrying"
        @click="onRetry"
      >
        {{ retrying ? 'Retrying...' : 'Retry' }}
      </button>
    </div>
  </section>
</template>
