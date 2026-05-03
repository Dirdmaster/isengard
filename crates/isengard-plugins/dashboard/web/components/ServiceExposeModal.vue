<template>
  <RoutingRuleEditModal
    :open="open"
    :rule="(seedRule as any)"
    :default-host-id="hostId"
    @update:open="emit('update:open', $event)"
  />
</template>

<script setup lang="ts">
import { computed } from 'vue'
import RoutingRuleEditModal from '~/components/RoutingRuleEditModal.vue'
import type { RoutingRule } from '~/composables/useRoutingRules'

const props = defineProps<{
  open: boolean
  hostId: string
  serviceName: string
  containerPort: number
}>()
const emit = defineEmits<{ (e: 'update:open', v: boolean): void }>()

const seedRule = computed<Partial<RoutingRule>>(() => ({
  service_name: props.serviceName,
  container_port: props.containerPort,
  source: 'ui',
}))
</script>
