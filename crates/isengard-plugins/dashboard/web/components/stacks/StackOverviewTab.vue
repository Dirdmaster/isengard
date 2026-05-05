<script setup lang="ts">
import type { Stack } from '~/stores/stacks'
import type { Service } from '~/stores/services'
import type { EventRow as EventRowType } from '~/stores/events'
import ServiceChip from '~/components/ServiceChip.vue'
import EventRow from '~/components/EventRow.vue'

interface Props {
  stack: Stack
  services: Service[]
  recentEvents: EventRowType[]
}

defineProps<Props>()
defineEmits<{ expose: [hostId: string, serviceName: string, port: number] }>()
</script>

<template>
  <div class="grid grid-cols-2 gap-6 p-6">
    <section>
      <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Services</h2>
      <div class="flex flex-wrap gap-2 items-center">
        <template v-for="svc in services" :key="svc.name">
          <div class="flex items-center gap-1">
            <ServiceChip :name="svc.name" :state="svc.state" />
            <button
              class="text-[10px] text-iso-text-muted hover:text-iso-info underline px-1"
              @click="$emit('expose', svc.host_id, svc.name, 0)"
            >
              Expose
            </button>
          </div>
        </template>
        <span v-if="services.length === 0" class="text-sm text-iso-text-faint">
          No services reported (waiting for next heartbeat).
        </span>
      </div>
    </section>

    <section>
      <h2 class="text-xs uppercase tracking-wider text-iso-text-faint mb-3">Recent events</h2>
      <div class="space-y-1">
        <EventRow
          v-for="e in recentEvents"
          :key="e.id"
          :event="e"
          :selected="false"
        />
        <span v-if="recentEvents.length === 0" class="text-sm text-iso-text-faint">
          No recent events for this stack's host.
        </span>
      </div>
    </section>
  </div>
</template>
