import { ref, onMounted } from 'vue'

export interface RoutingRule {
  id: number
  fleet: string
  host_id: string
  service_name: string
  container_port: number
  public_hostname: string
  protocol: string
  adapter: string
  tls_mode: 'edge' | 'acme' | 'manual'
  healthcheck_path: string | null
  healthcheck_interval_secs: number
  state: 'pending' | 'active' | 'draining' | 'failed'
  source: 'ui' | 'label' | 'imported'
  source_container_id: string | null
}

const PATH = '/routing/rules'

export function useRoutingRules() {
  const api = useApi()
  const rules = ref<RoutingRule[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      rules.value = await api.get<RoutingRule[]>(PATH)
    } catch (e: any) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  async function createRule(body: Partial<RoutingRule>) {
    await api.post(PATH, body)
    await refresh()
  }

  async function updateRule(id: number, body: Partial<RoutingRule>) {
    await api.patch(`${PATH}/${id}`, body)
    await refresh()
  }

  async function deleteRule(id: number) {
    await api.delete(`${PATH}/${id}`)
    await refresh()
  }

  onMounted(refresh)

  return { rules, loading, error, refresh, createRule, updateRule, deleteRule }
}
