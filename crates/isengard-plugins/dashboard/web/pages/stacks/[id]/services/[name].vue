<script setup lang="ts">
import { computed } from 'vue'
import EmptyState from '~/components/EmptyState.vue'
import EventRow from '~/components/EventRow.vue'
import KvRow from '~/components/KvRow.vue'
import LogsPanel from '~/components/LogsPanel.vue'
import StatusPill from '~/components/StatusPill.vue'
import EffectivePolicyPreview from '~/components/policies/EffectivePolicyPreview.vue'
import TopBar from '~/components/TopBar.vue'
import { useServiceDetail } from '~/composables/useServiceDetail'
import { useToast } from '~/composables/useToast'

const route = useRoute()
const stackId = computed(() => route.params.id as string)
const serviceName = computed(() => route.params.name as string)

const { data, loading, error, status, reload } = useServiceDetail(
  stackId,
  serviceName,
)

const service = computed(() => data.value?.service ?? null)

type ChipState = 'success' | 'warn' | 'error' | 'info' | 'neutral'
const statusInfo = computed<{ state: ChipState; label: string; icon?: string }>(() => {
  const s = service.value?.state
  switch (s) {
    case 'running':
      return { state: 'success', label: 'running', icon: 'lucide:check-circle-2' }
    case 'restarting':
      return { state: 'warn', label: 'restarting', icon: 'lucide:loader' }
    case 'stopped':
      return { state: 'error', label: 'stopped', icon: 'lucide:x-circle' }
    default:
      return { state: 'neutral', label: 'unknown' }
  }
})

const formattedLastSeen = computed(() => {
  if (!service.value?.last_seen_at) return ''
  return new Date(service.value.last_seen_at).toLocaleString()
})

const stackName = computed(() => {
  // The detail endpoint does not return the stack name today; recover from
  // a routing rule (stack-scoped rules carry the stack id) or fall back to
  // the path. The page header's breadcrumb only needs the human label.
  return ''
})

async function forceUpdate() {
  if (!stackId.value) return
  try {
    const api = useApi()
    await api.post(`/stacks/${stackId.value}/actions/force-update`, {})
    useToast().success('Force update queued for stack')
  } catch (e) {
    useToast().error(
      `Force update failed: ${e instanceof Error ? e.message : String(e)}`,
    )
  }
}

const policyContext = computed(() => ({
  fleet: undefined,
  stack: undefined,
  service: service.value?.name,
  host_id: service.value?.host_id,
}))

function deploymentStateChip(state: string): { state: ChipState; label: string; icon?: string } {
  switch (state) {
    case 'done':
      return { state: 'success', label: 'done', icon: 'lucide:check-circle-2' }
    case 'failed':
      return { state: 'error', label: 'failed', icon: 'lucide:x-circle' }
    case 'aborted':
      return { state: 'warn', label: 'aborted', icon: 'lucide:octagon-alert' }
    default:
      return { state: 'info', label: state, icon: 'lucide:loader' }
  }
}

function formatTime(iso: string | null): string {
  if (!iso) return ''
  return new Date(iso).toLocaleString()
}
</script>

<template>
  <div class="flex-1 flex flex-col min-h-0">
    <TopBar />

    <div v-if="loading && !data" class="p-6 text-iso-text-muted">
      Loading service detail...
    </div>

    <div v-else-if="status === 404" class="flex-1 flex">
      <EmptyState
        icon="alert-circle"
        title="Service not found"
        description="The stack or service no longer exists. It may have been removed out-of-band, or the URL is stale."
      >
        <template #cta>
          <NuxtLink
            :to="`/stacks/${stackId}`"
            class="px-3 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:border-iso-info hover:text-iso-info"
          >
            Back to stack
          </NuxtLink>
        </template>
      </EmptyState>
    </div>

    <div v-else-if="error" class="p-6 text-iso-error">
      Error loading service: {{ error }}
      <button class="ml-3 underline text-iso-info" @click="reload">Retry</button>
    </div>

    <div v-else-if="data && service" class="flex-1 overflow-y-auto">
      <!-- Header -->
      <header class="flex items-start justify-between p-6 border-b border-iso-border-subtle gap-4">
        <div class="min-w-0">
          <nav class="text-xs text-iso-text-muted flex items-center gap-1">
            <NuxtLink to="/stacks" class="hover:text-iso-text-primary">Stacks</NuxtLink>
            <span class="text-iso-text-faint">/</span>
            <NuxtLink :to="`/stacks/${stackId}`" class="hover:text-iso-text-primary">
              {{ stackName || stackId }}
            </NuxtLink>
            <span class="text-iso-text-faint">/</span>
            <span class="font-mono text-iso-text-primary truncate">{{ service.name }}</span>
          </nav>
          <h1 class="font-mono text-2xl mt-1 flex items-center gap-3">
            {{ service.name }}
            <StatusPill
              :state="statusInfo.state"
              :label="statusInfo.label"
              :icon="statusInfo.icon"
              size="sm"
            />
          </h1>
          <div class="text-xs text-iso-text-muted mt-1.5 flex items-center gap-2 flex-wrap">
            <span>on {{ service.hostname || service.host_id.slice(0, 8) }}</span>
            <span class="text-iso-text-faint">·</span>
            <span class="font-mono">{{ service.image }}</span>
            <span v-if="data.other_instances.length > 0" class="text-iso-text-faint">·</span>
            <span v-if="data.other_instances.length > 0">
              {{ data.other_instances.length + 1 }} instances across hosts
            </span>
          </div>
        </div>
        <div class="flex items-center gap-2 shrink-0">
          <button
            class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:border-iso-success hover:text-iso-success"
            @click="forceUpdate"
          >
            <Icon name="lucide:zap" class="w-3.5 h-3.5 mr-1.5 inline" />
            Force update
          </button>
          <button
            class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-subtle text-xs text-iso-text-faint cursor-not-allowed"
            disabled
            title="Pause updates lands with the policy editor follow-up (see issue #57)"
          >
            <Icon name="lucide:pause" class="w-3.5 h-3.5 mr-1.5 inline" />
            Pause updates
          </button>
          <NuxtLink
            :to="`/settings/networking`"
            class="px-2.5 py-1.5 rounded-iso-md bg-iso-bg-elevated border border-iso-border-strong text-xs text-iso-text-secondary hover:border-iso-info hover:text-iso-info"
          >
            <Icon name="lucide:network" class="w-3.5 h-3.5 mr-1.5 inline" />
            Open routing
          </NuxtLink>
        </div>
      </header>

      <!-- Two-column body -->
      <div class="grid grid-cols-1 lg:grid-cols-[1fr_2fr] gap-4 p-6">

        <!-- LEFT COLUMN: metadata + policy + last deployment -->
        <div class="flex flex-col gap-4">
          <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
            <div class="px-4 py-3 border-b border-iso-border-subtle">
              <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
                METADATA
              </span>
            </div>
            <div class="p-4 flex flex-col gap-2">
              <KvRow label="Service" :value="service.name" mono />
              <KvRow label="Image" :value="service.image" mono />
              <KvRow label="State" :value="service.state" />
              <KvRow
                label="Host"
                :value="service.hostname || service.host_id.slice(0, 12)"
                mono
              />
              <KvRow label="Last seen" :value="formattedLastSeen" />
              <KvRow
                label="Deploy"
                :value="service.deploy_strategy_override || 'auto'"
                mono
              />
            </div>
          </section>

          <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
            <EffectivePolicyPreview
              :fleet="policyContext.fleet"
              :stack="policyContext.stack"
              :service="policyContext.service"
              :host_id="policyContext.host_id"
            />
          </section>

          <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
            <div class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between">
              <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
                LAST DEPLOYMENT
              </span>
              <span
                v-if="data.last_deployment"
                class="text-[10px]"
              >
                <StatusPill
                  v-bind="deploymentStateChip(data.last_deployment.state)"
                  size="xs"
                />
              </span>
            </div>
            <div v-if="data.last_deployment" class="p-4 flex flex-col gap-2 text-xs">
              <KvRow
                label="Strategy"
                :value="data.last_deployment.strategy"
                mono
              />
              <KvRow
                label="State"
                :value="data.last_deployment.state"
                mono
              />
              <KvRow
                label="Started"
                :value="formatTime(data.last_deployment.created_at)"
              />
              <KvRow
                v-if="data.last_deployment.finished_at"
                label="Finished"
                :value="formatTime(data.last_deployment.finished_at)"
              />
              <div
                v-if="data.last_deployment.error"
                class="font-mono text-[11px] text-iso-error mt-2"
              >
                {{ data.last_deployment.error }}
              </div>
            </div>
            <div v-else class="p-4 text-xs text-iso-text-muted">
              No deployment history for this service yet.
            </div>
          </section>
        </div>

        <!-- RIGHT COLUMN: logs placeholder + routing + events + other instances -->
        <div class="flex flex-col gap-4">
          <!-- Live logs panel (Phase 13B). Mounts a WebSocket against
               /api/v1/services/:stack_id/:service_name/logs/ws and renders
               backfill + live tail with pause/resume + filter + per-host tabs. -->
          <LogsPanel
            :stack-id="stackId"
            :service-name="serviceName"
          />

          <!-- Routing rules -->
          <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
            <div class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between">
              <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
                ROUTING RULES ({{ data.routing_rules.length }})
              </span>
              <NuxtLink
                to="/settings/networking"
                class="text-[11px] text-iso-info hover:underline"
              >
                Manage in settings
              </NuxtLink>
            </div>
            <div v-if="data.routing_rules.length === 0" class="p-4 text-xs text-iso-text-muted">
              No routing rules attached. Add one from Settings or via an
              <code class="font-mono text-iso-text-secondary">isengard.expose</code>
              container label.
            </div>
            <div v-else class="divide-y divide-iso-border-subtle">
              <div
                v-for="r in data.routing_rules"
                :key="r.id"
                class="px-4 py-2.5 grid grid-cols-[1fr_120px_80px_70px] gap-3 text-xs items-center"
              >
                <span class="font-medium text-iso-text-primary truncate">
                  {{ r.public_hostname }}
                </span>
                <span class="font-mono text-iso-text-muted truncate">
                  {{ r.service_name }}:{{ r.container_port }}
                </span>
                <span class="text-iso-text-muted">{{ r.adapter }}</span>
                <span
                  :class="r.state === 'active'
                    ? 'text-iso-success'
                    : r.state === 'failed'
                      ? 'text-iso-error'
                      : 'text-iso-warn'"
                >
                  {{ r.state }}
                </span>
              </div>
            </div>
          </section>

          <!-- Recent events -->
          <section class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden">
            <div class="px-4 py-3 border-b border-iso-border-subtle flex items-center justify-between">
              <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
                RECENT EVENTS ({{ data.recent_events.length }})
              </span>
              <NuxtLink to="/events" class="text-[11px] text-iso-info hover:underline">
                View all events
              </NuxtLink>
            </div>
            <div v-if="data.recent_events.length === 0" class="p-4 text-xs text-iso-text-muted">
              No recent events for this service.
            </div>
            <div v-else class="divide-y divide-iso-border-subtle max-h-[480px] overflow-y-auto">
              <EventRow
                v-for="e in data.recent_events"
                :key="e.id"
                :event="(e as any)"
                :selected="false"
              />
            </div>
          </section>

          <!-- Other instances -->
          <section
            v-if="data.other_instances.length > 0"
            class="rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden"
          >
            <div class="px-4 py-3 border-b border-iso-border-subtle">
              <span class="text-[10px] font-semibold tracking-wider text-iso-text-muted">
                OTHER INSTANCES ({{ data.other_instances.length }})
              </span>
            </div>
            <div class="divide-y divide-iso-border-subtle">
              <div
                v-for="o in data.other_instances"
                :key="o.id"
                class="px-4 py-2.5 grid grid-cols-[1fr_140px_120px] gap-3 text-xs items-center"
              >
                <span class="font-mono text-iso-text-primary truncate">
                  {{ o.hostname || o.host_id.slice(0, 12) }}
                </span>
                <span class="font-mono text-iso-text-muted truncate">{{ o.image }}</span>
                <span
                  :class="o.state === 'running'
                    ? 'text-iso-success'
                    : o.state === 'stopped'
                      ? 'text-iso-error'
                      : 'text-iso-warn'"
                >
                  {{ o.state }}
                </span>
              </div>
            </div>
          </section>
        </div>
      </div>
    </div>
  </div>
</template>
