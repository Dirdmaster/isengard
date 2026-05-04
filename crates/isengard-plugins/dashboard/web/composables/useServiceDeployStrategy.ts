import { ref } from 'vue'

/**
 * Mirror of `dashboard::deployments::ServiceDeployStrategyDto`.
 * See `crates/isengard-plugins/dashboard/src/deployments.rs`.
 */
export interface ServiceDeployStrategyDto {
  service_id: number
  host_id: string
  stack_id: number | null
  stack_name: string | null
  service_name: string
  override_value: string | null
}

/** UI-side strategy values. `auto` clears the persisted override. */
export type DeployStrategyChoice = 'auto' | 'blue-green' | 'in-place'

/**
 * Reactive view over `GET /services/deploy-strategy` plus a `setOverride`
 * helper that PUTs the new value and refreshes the list. Used by the
 * `DeploymentsSettings` tab.
 */
export function useServiceDeployStrategy() {
  const api = useApi()
  const items = ref<ServiceDeployStrategyDto[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      items.value = await api.get<ServiceDeployStrategyDto[]>('/services/deploy-strategy')
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  async function setOverride(serviceId: number, choice: DeployStrategyChoice) {
    await api.put<void>(`/services/${serviceId}/deploy-strategy`, {
      override_value: choice,
    })
    await refresh()
  }

  return { items, loading, error, refresh, setOverride }
}
