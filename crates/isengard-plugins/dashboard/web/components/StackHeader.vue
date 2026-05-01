<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Props {
  stack: Stack
  hostHostname: string
  fleet: string
}

defineProps<Props>()
defineEmits<{ 'force-update': [] }>()
</script>

<template>
  <header class="flex items-center justify-between p-6 border-b border-iso-border-subtle">
    <div>
      <NuxtLink to="/stacks" class="text-xs text-iso-text-muted hover:text-iso-text-primary">
        ← Stacks
      </NuxtLink>
      <h1 class="font-mono text-xl mt-1 flex items-center gap-2">
        <Icon name="lucide:layers" class="w-5 h-5 text-iso-text-muted" />
        {{ stack.name }}
      </h1>
      <div class="text-sm text-iso-text-muted mt-1">
        on <NuxtLink :to="`/stacks?host_id=${stack.host_id}`" class="hover:text-iso-text-primary">{{ hostHostname }}</NuxtLink>
        · fleet {{ fleet }}
        · source {{ stack.source }}
      </div>
    </div>
    <Button
      variant="outline"
      class="border-iso-border-subtle hover:border-iso-success hover:text-iso-success"
      @click="$emit('force-update')"
    >
      <Icon name="lucide:zap" class="w-3.5 h-3.5 mr-1.5" />
      Force update stack
    </Button>
  </header>
</template>
