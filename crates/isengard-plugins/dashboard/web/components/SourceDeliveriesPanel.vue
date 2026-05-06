<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import {
  useWebhooks,
  type DeliveryStatus,
  type DeliverySource,
  type WebhookDeliveryDto,
} from '~/composables/useWebhooks'

/**
 * Phase 12b/c: list deliveries filtered by source (lifecycle / gate).
 *
 * Backs the new sub-tabs in `WebhooksSettings.vue`. Reuses the same row
 * shape as `WebhookDeliveriesPanel.vue` but pulls from the cross-source
 * `/webhooks/deliveries?source=` endpoint instead of the per-webhook one.
 */
const props = defineProps<{ source: DeliverySource; limit?: number }>()

const { listDeliveriesBySource } = useWebhooks()

const rows = ref<WebhookDeliveryDto[]>([])
const loading = ref(false)
const error = ref<string | null>(null)

async function refresh() {
  loading.value = true
  error.value = null
  try {
    rows.value = await listDeliveriesBySource(props.source, props.limit)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(refresh)
watch(() => props.source, refresh)

const isEmpty = computed(() => !loading.value && !error.value && rows.value.length === 0)

function statusClass(s: DeliveryStatus): string {
  switch (s) {
    case 'success':
      return 'text-iso-success'
    case 'failed':
    case 'exhausted':
      return 'text-iso-error'
    case 'pending':
    default:
      return 'text-iso-text-muted'
  }
}

function emptyHelp(s: DeliverySource): string {
  if (s === 'lifecycle') {
    return 'No lifecycle deliveries yet. Containers carrying isengard.hooks.* labels will fire deliveries on deploy.'
  }
  return 'No gate evaluations yet. Configure a policy with external_gate.url to start using gates.'
}

defineExpose({ refresh })
</script>

<template>
  <div class="flex flex-col gap-2">
    <div class="flex items-center justify-between">
      <p class="text-xs text-iso-text-muted">
        <template v-if="source === 'lifecycle'">
          Lifecycle hook deliveries from
          <code class="text-iso-text-primary">isengard.hooks.*</code> labels.
        </template>
        <template v-else>
          External-action gate evaluations from policy <code class="text-iso-text-primary">external_gate</code>.
        </template>
      </p>
      <button
        class="text-[11px] text-iso-text-muted hover:text-iso-text-primary px-2 py-1 rounded-iso-sm border border-iso-border-subtle"
        :disabled="loading"
        @click="refresh"
      >
        {{ loading ? 'Loading...' : 'Refresh' }}
      </button>
    </div>

    <div
      v-if="error"
      class="rounded-iso-md border border-iso-error/40 bg-iso-error-soft px-3 py-2 text-xs text-iso-error"
    >
      {{ error }}
    </div>

    <div
      v-else-if="isEmpty"
      class="rounded-iso-md border border-dashed border-iso-border-strong bg-iso-bg-elevated p-4 text-center text-iso-text-muted text-xs"
    >
      {{ emptyHelp(source) }}
    </div>

    <table v-else class="w-full text-xs">
      <thead>
        <tr class="text-iso-text-muted">
          <th class="text-left font-normal pb-1">Status</th>
          <th class="text-left font-normal pb-1">Event</th>
          <th class="text-left font-normal pb-1">URL</th>
          <th class="text-left font-normal pb-1">Attempts</th>
          <th class="text-left font-normal pb-1">When</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="row in rows"
          :key="row.id"
          class="border-t border-iso-border-subtle"
        >
          <td :class="['py-1 pr-3 font-mono', statusClass(row.status)]">
            {{ row.status }}
          </td>
          <td class="py-1 pr-3 font-mono text-iso-text-primary">
            {{ row.eventKind }}
          </td>
          <td class="py-1 pr-3 font-mono text-iso-text-muted truncate max-w-[18rem]" :title="row.url || ''">
            {{ row.url || '-' }}
          </td>
          <td class="py-1 pr-3 text-iso-text-muted">
            {{ row.attempts }}
          </td>
          <td class="py-1 text-iso-text-muted">
            {{ row.lastAttemptAt || row.createdAt }}
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
