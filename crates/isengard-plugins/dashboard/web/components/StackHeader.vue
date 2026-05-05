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
</script>

<template>
  <header class="flex items-start justify-between p-6 border-b border-iso-border-subtle gap-4">
    <div class="min-w-0">
      <nav class="text-xs text-iso-text-muted flex items-center gap-1">
        <NuxtLink to="/stacks" class="hover:text-iso-text-primary">Stacks</NuxtLink>
        <span class="text-iso-text-faint">/</span>
        <span class="font-mono text-iso-text-primary truncate">{{ stack.name }}</span>
      </nav>
      <h1 class="font-mono text-xl mt-1 flex items-center gap-2">
        <Icon name="lucide:layers" class="w-5 h-5 text-iso-text-muted" />
        {{ stack.name }}
        <StatusPill
          :state="status.state"
          :label="status.label"
          :icon="status.icon"
          size="sm"
        />
      </h1>
      <div class="text-sm text-iso-text-muted mt-1">
        on <NuxtLink :to="`/stacks?host_id=${stack.host_id}`" class="hover:text-iso-text-primary">{{ hostHostname }}</NuxtLink>
        · fleet {{ fleet }}
        · source {{ stack.source }}
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
