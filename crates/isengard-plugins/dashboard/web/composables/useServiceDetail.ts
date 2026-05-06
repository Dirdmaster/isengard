import { ref, watch, type Ref } from 'vue'
import type { ResolvedPolicy } from '~/composables/useEffectivePolicy'

/**
 * Mirror of `crates/isengard-plugins/dashboard/src/dto.rs::ServiceDto` with
 * the Phase 13A enrichments (hostname, last_seen_at, deploy_strategy_override).
 */
export interface ServiceDto {
  id: string
  host_id: string
  hostname?: string
  stack_id: string | null
  name: string
  image: string
  state: 'running' | 'stopped' | 'restarting' | 'unknown'
  last_seen_at: string
  deploy_strategy_override: string | null
}

export interface DeploymentSummaryDto {
  id: string
  state: string
  service_name: string
  strategy: string
  blue_digest: string
  green_digest: string
  finished_at: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface RecentEventDto {
  id: number
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
}

export interface RoutingRuleSummary {
  id: number
  service_name: string
  container_port: number
  public_hostname: string
  protocol: string
  adapter: string
  tls_mode: string
  state: string
  source: string
  healthcheck_path: string | null
}

export interface ServiceDetailDto {
  service: ServiceDto
  other_instances: ServiceDto[]
  effective_policy: ResolvedPolicy
  last_deployment: DeploymentSummaryDto | null
  recent_events: RecentEventDto[]
  routing_rules: RoutingRuleSummary[]
}

/**
 * Lazy fetcher for `/api/v1/services/:stack_id/:service_name`. Re-fetches
 * when stack_id or service_name change.
 */
export function useServiceDetail(
  stackId: Ref<string>,
  serviceName: Ref<string>,
) {
  const data = ref<ServiceDetailDto | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)
  const status = ref<number | null>(null)

  async function load() {
    if (!stackId.value || !serviceName.value) return
    loading.value = true
    error.value = null
    status.value = null
    try {
      const api = useApi()
      data.value = await api.get<ServiceDetailDto>(
        `/services/${stackId.value}/${encodeURIComponent(serviceName.value)}`,
      )
      status.value = 200
    } catch (e: unknown) {
      // $fetch errors carry `statusCode` on the FetchError shape.
      const ex = e as { statusCode?: number; message?: string }
      status.value = ex?.statusCode ?? null
      error.value = ex?.message ?? String(e)
      data.value = null
    } finally {
      loading.value = false
    }
  }

  watch(
    [stackId, serviceName],
    () => {
      load()
    },
    { immediate: true },
  )

  return { data, loading, error, status, reload: load }
}
