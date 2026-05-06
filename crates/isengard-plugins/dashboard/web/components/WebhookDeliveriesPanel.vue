<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import {
  useWebhooks,
  type DeliveryStatus,
  type WebhookDeliveryDto,
} from '~/composables/useWebhooks'

/**
 * Per-webhook deliveries panel. Loads on mount, refreshes on status filter
 * change. Shipped in Phase 12a (#53).
 */

const props = defineProps<{ webhookId: number }>()

const { listDeliveries } = useWebhooks()

const rows = ref<WebhookDeliveryDto[]>([])
const loading = ref(false)
const error = ref<string | null>(null)
const statusFilter = ref<DeliveryStatus | ''>('')

async function refresh() {
  loading.value = true
  error.value = null
  try {
    rows.value = await listDeliveries(props.webhookId, statusFilter.value || undefined)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(refresh)
watch(() => props.webhookId, refresh)
watch(statusFilter, refresh)

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

function fmt(t?: string): string {
  if (!t) return ''
  try {
    return new Date(t).toLocaleString()
  } catch {
    return t
  }
}
</script>

<template>
  <div class="px-4 py-3 flex flex-col gap-2">
    <div class="flex items-center gap-2">
      <span class="text-[11px] font-semibold text-iso-text-muted">Status:</span>
      <select
        v-model="statusFilter"
        class="bg-iso-bg-base border border-iso-border-subtle rounded-iso-sm px-2 py-1 text-xs text-iso-text-primary"
      >
        <option value="">all</option>
        <option value="pending">pending</option>
        <option value="success">success</option>
        <option value="failed">failed</option>
        <option value="exhausted">exhausted</option>
      </select>
      <button
        class="ml-auto text-[11px] text-iso-text-muted hover:text-iso-text-primary"
        @click="refresh"
      >
        Refresh
      </button>
    </div>

    <div v-if="loading" class="text-[11px] text-iso-text-muted py-3 text-center">
      Loading deliveries...
    </div>
    <div v-else-if="error" class="text-[11px] text-iso-error py-3">{{ error }}</div>
    <div v-else-if="isEmpty" class="text-[11px] text-iso-text-muted py-3 text-center">
      No deliveries yet.
    </div>

    <table v-else class="w-full text-[11px] font-mono">
      <thead class="text-iso-text-muted text-left">
        <tr class="border-b border-iso-border-subtle">
          <th class="py-1 font-semibold">time</th>
          <th class="py-1 font-semibold">event</th>
          <th class="py-1 font-semibold">status</th>
          <th class="py-1 font-semibold text-right">attempts</th>
          <th class="py-1 font-semibold">last error</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="r in rows"
          :key="r.id"
          class="border-b border-iso-border-subtle/60 last:border-b-0"
        >
          <td class="py-1 text-iso-text-muted">{{ fmt(r.createdAt) }}</td>
          <td class="py-1 text-iso-text-primary">{{ r.eventKind }}</td>
          <td class="py-1" :class="statusClass(r.status)">{{ r.status }}</td>
          <td class="py-1 text-right text-iso-text-primary">{{ r.attempts }}</td>
          <td class="py-1 text-iso-text-muted truncate max-w-[20rem]">
            {{ r.lastError || '' }}
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
