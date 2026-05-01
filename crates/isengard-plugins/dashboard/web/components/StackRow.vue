<script setup lang="ts">
import type { Stack } from '~/stores/stacks'

interface Props {
  stack: Stack
  services: { name: string; state?: 'running' | 'stopped' | 'restarting' | 'unknown' }[]
}

defineProps<Props>()
defineEmits<{ click: [stack: Stack] }>()
</script>

<template>
  <div
    class="flex items-center gap-3 py-2 px-3 rounded hover:bg-iso-bg-elevated cursor-pointer"
    @click="$emit('click', stack)"
  >
    <Icon name="lucide:layers" class="w-4 h-4 text-iso-text-muted shrink-0" />
    <div class="flex-1 min-w-0">
      <div class="font-mono text-sm">{{ stack.name }}</div>
      <div class="text-[10px] text-iso-text-faint">{{ services.length }} services</div>
    </div>
    <div class="flex items-center gap-1.5 flex-wrap justify-end max-w-[60%]">
      <ServiceChip
        v-for="svc in services"
        :key="svc.name"
        :name="svc.name"
        :state="svc.state"
      />
    </div>
  </div>
</template>
