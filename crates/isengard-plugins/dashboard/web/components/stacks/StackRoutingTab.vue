<template>
  <div class="p-6">
    <!--
      Per-stack routing rules. Read-only view: edits live in
      Settings > Networking. The list endpoint is fleet-wide today, so we
      filter client-side by host_id (always) and stack_id (when the rule
      carries one). A backend-side filter can replace this later without
      touching the visual layout.
    -->
    <div class="flex flex-col gap-4">
      <div class="flex flex-col gap-0.5">
        <h3 class="text-sm font-semibold text-iso-text-primary">Routing rules</h3>
        <span class="text-[11px] text-iso-text-muted">
          Proxy routes for services in this stack. Edits live in Settings.
        </span>
      </div>

      <div v-if="loading && filteredRules.length === 0" class="text-sm text-iso-text-muted">
        Loading routing rules...
      </div>

      <div v-else-if="error" class="text-sm text-iso-error">
        Error loading routing rules: {{ error }}
      </div>

      <EmptyState
        v-else-if="filteredRules.length === 0"
        icon="route"
        title="No routing rules"
        description="Add label `isengard.expose.host=...` to a service to enable proxy routing."
      >
        <template #cta>
          <NuxtLink
            to="/settings?tab=networking&subtab=proxy"
            class="text-xs px-3 py-1.5 rounded-md border border-iso-border-subtle text-iso-info hover:border-iso-info hover:bg-iso-info/10 transition-colors"
          >
            Open global routing rules
          </NuxtLink>
        </template>
      </EmptyState>

      <div
        v-else
        class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden"
      >
        <div
          class="grid grid-cols-[minmax(160px,1.2fr)_minmax(220px,1.5fr)_110px_90px_120px_120px_110px] px-4 py-2.5 text-[10px] font-semibold tracking-wider text-iso-text-muted border-b border-iso-border-subtle"
        >
          <div>SERVICE</div>
          <div>PUBLIC HOST</div>
          <div>PORT</div>
          <div>PROTOCOL</div>
          <div>TLS MODE</div>
          <div>STATE</div>
          <div>SOURCE</div>
        </div>

        <div
          v-for="(r, i) in filteredRules"
          :key="r.id"
          class="grid grid-cols-[minmax(160px,1.2fr)_minmax(220px,1.5fr)_110px_90px_120px_120px_110px] px-4 py-3 text-xs items-center"
          :class="i < filteredRules.length - 1 ? 'border-b border-iso-border-subtle' : ''"
        >
          <div class="font-medium text-iso-text-primary truncate">{{ r.service_name }}</div>
          <div class="font-mono text-iso-text-muted truncate">{{ r.public_hostname }}</div>
          <div class="font-mono text-iso-text-muted">{{ r.container_port }}</div>
          <div class="text-iso-text-muted">{{ r.protocol }}</div>
          <div :class="tlsClass(r.tls_mode)">{{ r.tls_mode }}</div>
          <div class="flex items-center gap-2">
            <div class="w-2 h-2 rounded-full" :class="stateDot(r.state)"></div>
            <span :class="stateText(r.state)">{{ r.state }}</span>
          </div>
          <div :class="sourceClass(r.source)">{{ r.source }}</div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRoutingRules, type RoutingRule } from '~/composables/useRoutingRules'
import EmptyState from '~/components/EmptyState.vue'

const props = defineProps<{
  stackId: string
  hostId: string
}>()

const { rules, loading, error } = useRoutingRules()

const filteredRules = computed<RoutingRule[]>(() => {
  return rules.value.filter((r) => {
    // host_id is always required: a rule belongs to a host.
    if (r.host_id !== props.hostId) return false
    // stack_id may be null (fleet-wide rule) or set. When set, it must match
    // the current stack. JSON serialises StackId as a number; string-compare
    // to be defensive against numeric route params.
    if (r.stack_id !== null && r.stack_id !== undefined) {
      if (String(r.stack_id) !== String(props.stackId)) return false
    }
    return true
  })
})

function stateDot(s: string): string {
  if (s === 'active') return 'bg-iso-success'
  if (s === 'pending' || s === 'draining') return 'bg-iso-warn'
  if (s === 'failed') return 'bg-iso-error'
  return 'bg-iso-text-muted'
}

function stateText(s: string): string {
  if (s === 'active') return 'text-iso-success'
  if (s === 'pending' || s === 'draining') return 'text-iso-warn'
  if (s === 'failed') return 'text-iso-error'
  return 'text-iso-text-muted'
}

function tlsClass(mode: string): string {
  if (!mode || mode === 'manual') return 'text-iso-text-muted'
  return 'text-iso-success'
}

function sourceClass(s: string): string {
  if (s === 'ui') return 'text-iso-info'
  if (s === 'label') return 'text-iso-warn'
  return 'text-iso-text-muted'
}
</script>
