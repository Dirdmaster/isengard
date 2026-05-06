import { ref } from 'vue'

/**
 * Mirror of `dashboard::webhooks::WebhookDto` (camelCase).
 * See `crates/isengard-plugins/dashboard/src/webhooks.rs`.
 */
export type DeliveryStatus = 'pending' | 'success' | 'failed' | 'exhausted'

export interface WebhookDto {
  id: number
  url: string
  secretMasked: string
  eventKinds: string
  enabled: boolean
  createdAt: string
  updatedAt: string
}

export interface WebhookCreatedDto extends WebhookDto {
  /** Plaintext secret. Returned exactly once on create. */
  secret: string
}

export interface WebhookDeliveryDto {
  id: number
  webhookId: number
  eventKind: string
  status: DeliveryStatus
  attempts: number
  lastAttemptAt?: string
  lastError?: string
  nextRetryAt?: string
  createdAt: string
}

export interface CreateWebhookBody {
  url: string
  secret?: string
  eventKinds?: string
  enabled?: boolean
}

export interface UpdateWebhookBody {
  url?: string
  secret?: string
  eventKinds?: string
  enabled?: boolean
}

export function useWebhooks() {
  const api = useApi()
  const webhooks = ref<WebhookDto[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      webhooks.value = await api.get<WebhookDto[]>('/webhooks')
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function createWebhook(body: CreateWebhookBody): Promise<WebhookCreatedDto> {
    const created = await api.post<WebhookCreatedDto>('/webhooks', body)
    await refresh()
    return created
  }

  async function updateWebhook(id: number, body: UpdateWebhookBody): Promise<WebhookDto> {
    const updated = await api.put<WebhookDto>(`/webhooks/${id}`, body)
    await refresh()
    return updated
  }

  async function removeWebhook(id: number): Promise<void> {
    await api.delete(`/webhooks/${id}`)
    await refresh()
  }

  async function listDeliveries(id: number, status?: DeliveryStatus): Promise<WebhookDeliveryDto[]> {
    const path = status
      ? `/webhooks/${id}/deliveries?status=${encodeURIComponent(status)}`
      : `/webhooks/${id}/deliveries`
    return api.get<WebhookDeliveryDto[]>(path)
  }

  async function sendTest(id: number): Promise<WebhookDeliveryDto> {
    return api.post<WebhookDeliveryDto>(`/webhooks/${id}/test`)
  }

  return {
    webhooks,
    loading,
    error,
    refresh,
    createWebhook,
    updateWebhook,
    removeWebhook,
    listDeliveries,
    sendTest,
  }
}
