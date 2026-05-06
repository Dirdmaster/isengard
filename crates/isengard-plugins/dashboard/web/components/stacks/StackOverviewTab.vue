<script setup lang="ts">
import type { Stack } from '~/stores/stacks'
import type { Service } from '~/stores/services'
import type { EventRow as EventRowType } from '~/stores/events'
import EventRow from '~/components/EventRow.vue'
import EffectivePolicyPreview from '~/components/policies/EffectivePolicyPreview.vue'

interface Props {
  stack: Stack
  services: Service[]
  recentEvents: EventRowType[]
  /**
   * Fleet name for the stack's host. Used to scope the
   * `<EffectivePolicyPreview />` query per service. Optional: when missing
   * (host not yet hydrated, or unknown fleet) the preview is omitted.
   */
  fleet?: string
}

defineProps<Props>()
defineEmits<{ expose: [hostId: string, serviceName: string, port: number] }>()

function dotColor(state: Service['state']) {
  switch (state) {
    case 'running': return 'bg-iso-success'
    case 'restarting': return 'bg-iso-warn'
    case 'stopped': return 'bg-iso-error'
    default: return 'bg-iso-text-muted'
  }
}

function stateLabel(state: Service['state']) {
  switch (state) {
    case 'running': return 'running'
    case 'restarting': return 'restarting'
    case 'stopped': return 'stopped'
    default: return 'unknown'
  }
}

function stateClasses(state: Service['state']) {
  switch (state) {
    case 'running': return 'text-iso-success'
    case 'restarting': return 'text-iso-warn'
    case 'stopped': return 'text-iso-error'
    default: return 'text-iso-text-muted'
  }
}
</script>

<template>
  <div class="grid grid-cols-[1.4fr_1fr] gap-4 p-6">
    <!-- Services -->
    <section class="flex flex-col gap-3">
      <div class="flex items-center justify-between">
        <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
          SERVICES ({{ services.length }})
        </span>
      </div>

      <template v-if="services.length === 0">
        <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated p-6 text-sm text-iso-text-muted">
          No services reported yet (waiting for next agent heartbeat).
        </div>
      </template>

      <template v-else>
        <div
          v-for="svc in services"
          :key="`${svc.host_id}-${svc.name}`"
          class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden"
        >
          <NuxtLink
            :to="`/stacks/${stack.id}/services/${encodeURIComponent(svc.name)}`"
            class="px-4 py-3 flex items-center justify-between gap-3 hover:bg-iso-bg-row-hover transition-colors"
          >
            <div class="flex items-center gap-3 min-w-0">
              <span class="w-2 h-2 rounded-full shrink-0" :class="dotColor(svc.state)"></span>
              <span class="font-mono text-sm font-semibold text-iso-text-primary truncate">{{ svc.name }}</span>
              <span class="font-mono text-[11px] text-iso-text-secondary truncate">{{ svc.image }}</span>
            </div>
            <div class="flex items-center gap-3 shrink-0">
              <span class="text-[10px]" :class="stateClasses(svc.state)">{{ stateLabel(svc.state) }}</span>
              <button
                class="text-[10px] text-iso-text-muted hover:text-iso-info underline"
                @click.stop.prevent="$emit('expose', svc.host_id, svc.name, 0)"
              >
                Expose
              </button>
            </div>
          </NuxtLink>
          <EffectivePolicyPreview
            v-if="svc.name"
            :fleet="fleet"
            :stack="stack.name"
            :service="svc.name"
            :host_id="svc.host_id"
          />
        </div>
      </template>
    </section>

    <!-- Recent events -->
    <section class="flex flex-col gap-3">
      <div class="flex items-center justify-between">
        <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
          RECENT EVENTS
        </span>
        <span class="text-[10px] text-iso-text-faint">last 24h on this stack's host</span>
      </div>
      <div class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
        <div v-if="recentEvents.length === 0" class="p-4 text-sm text-iso-text-muted">
          No recent events for this stack's host.
        </div>
        <div v-else class="divide-y divide-iso-border-subtle">
          <EventRow
            v-for="e in recentEvents"
            :key="e.id"
            :event="e"
            :selected="false"
          />
        </div>
      </div>
    </section>
  </div>
</template>
