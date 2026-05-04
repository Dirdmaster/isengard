import { ref } from 'vue'

export interface AdapterConfig {
  host_id: string
  adapter: string
  config_json: Record<string, any>
  enabled: boolean
}

export interface TestResult {
  ok: boolean
  error?: string
  detail?: any
}

export function useAdapterConfig(hostId: string, adapter: string) {
  const api = useApi()
  const config = ref<AdapterConfig | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)
  const testResult = ref<TestResult | null>(null)
  const testing = ref(false)

  const path = `/networking/adapter-config/${hostId}/${adapter}`

  function is404(e: any): boolean {
    if (!e) return false
    if (typeof e.statusCode === 'number' && e.statusCode === 404) return true
    if (typeof e.response?.status === 'number' && e.response.status === 404) return true
    return String(e).includes('404')
  }

  async function load() {
    loading.value = true
    error.value = null
    try {
      config.value = await api.get<AdapterConfig>(path)
    } catch (e: any) {
      if (is404(e)) {
        config.value = null
      } else {
        error.value = e instanceof Error ? e.message : String(e)
      }
    } finally {
      loading.value = false
    }
  }

  async function save(config_json: Record<string, any>, enabled: boolean) {
    error.value = null
    try {
      config.value = await api.put<AdapterConfig>(path, { config_json, enabled })
    } catch (e: any) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    }
  }

  async function test() {
    testing.value = true
    try {
      testResult.value = await api.post<TestResult>(`${path}/test`)
    } catch (e: any) {
      testResult.value = { ok: false, error: e instanceof Error ? e.message : String(e) }
    } finally {
      testing.value = false
    }
  }

  return { config, loading, error, testResult, testing, load, save, test }
}
