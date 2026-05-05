<script setup lang="ts">
import { computed } from 'vue'
import type { Stack } from '~/stores/stacks'
import type { DeploymentDto } from '~/composables/useDeployments'
import StatusPill from '~/components/StatusPill.vue'

interface Props {
  stack: Stack
  hostHostname: string
  fleet: string
  /** Most recent active deployment for the stack, if any. */
  activeDeployment?: DeploymentDto | null
}

const props = withDefaults(defineProps<Props>(), {
  activeDeployment: null,
})

defineEmits<{
  'force-update': []
  'abort-deploy': [id: string]
}>()

type ChipState = 'success' | 'warn' | 'error' | 'info' | 'neutral'

const status = computed<{ state: ChipState; label: string; icon?: string }>(() => {
  const dep = props.activeDeployment
  if (dep) {
    switch (dep.state) {
      case 'failed':
        return { state: 'error', label: 'failed', icon: 'lucide:x-circle' }
      case 'aborted':
        return { state: 'warn', label: 'aborted', icon: 'lucide:octagon-alert' }
      case 'pending':
      case 'running':
      case 'switching':
      case 'draining':
        return { state: 'info', label: 'deploying', icon: 'lucide:loader' }
      case 'done':
        return { state: 'success', label: 'running', icon: 'lucide:check-circle-2' }
      default:
        return { state: 'neutral', label: dep.state }
    }
  }
  return { state: 'success', label: 'running', icon: 'lucide:check-circle-2' }
})

const canAbort = computed(() => {
  const dep = props.activeDeployment
  if (!dep) return false
  return ['pending', 'running', 'switching', 'draining'].includes(dep.state)
})

const statusDotColor = computed(() => {
  switch (status.value.state) {
    case 'success': return 'bg-iso-success'
    case 'warn': return 'bg-iso-warn'
    case 'error': return 'bg-iso-error'
    case 'info': return 'bg-iso-info'
    default: return 'bg-iso-text-muted'
  }
})
</script>

<template>
  <header class="flex items-start justify-between p-6 border-b border-iso-border-subtle gap-4">
    <div class="min-w-0">
      <nav class="text-xs text-iso-text-muted flex items-center gap-1">
        <NuxtLink to="/stacks" class="hover:text-iso-text-primary">Stacks</NuxtLink>
        <span class="text-iso-text-faint">/</span>
        <span class="font-mono text-iso-text-primary truncate">{{ stack.name }}</span>
      </nav>
      <h1 class="font-mono text-2xl mt-1 flex items-center gap-3">
        {{ stack.name }}
        <StatusPill
          :state="status.state"
          :label="status.label"
          :icon="status.icon"
          size="sm"
        />
      </h1>
      <div class="text-xs text-iso-text-muted mt-1.5 flex items-center gap-2 flex-wrap">
        <span class="flex items-center gap-1.5">
          <span class="w-1.5 h-1.5 rounded-full" :class="statusDotColor"></span>
          {{ status.label }}
        </span>
        <span class="text-iso-text-faint">·</span>
        <span>
          on <NuxtLink :to="`/stacks?host_id=${stack.host_id}`" class="hover:text-iso-text-primary">{{ hostHostname }}</NuxtLink>
        </span>
        <span class="text-iso-text-faint">·</span>
        <span>fleet {{ fleet }}</span>
        <span class="text-iso-text-faint">·</span>
        <span>source {{ stack.source }}</span>
      </div>
    </div>
    <div class="flex items-center gap-2 shrink-0">
      <Button
        v-if="canAbort && activeDeployment"
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-warn hover:text-iso-warn"
        @click="$emit('abort-deploy', activeDeployment.id)"
      >
        <Icon name="lucide:octagon-x" class="w-3.5 h-3.5 mr-1.5" />
        Abort deploy
      </Button>
      <Button
        variant="outline"
        class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
        @click="$emit('force-update')"
      >
        <Icon name="lucide:zap" class="w-3.5 h-3.5 mr-1.5" />
        Force update stack
      </Button>
    </div>
  </header>
</template>
