import { ref, type Ref } from 'vue'

export interface SparklineData {
  buckets: number[]
  range: string
  total: number
}

/**
 * Fetches the per-hour event-count sparkline for a host. Caller is responsible
 * for re-fetching on a schedule if a live sparkline is desired.
 */
export function useSparkline(hostId: Ref<string> | string) {
  const data = ref<SparklineData | null>(null)
  const loading = ref(false)

  async function fetchSparkline(range = '24h') {
    const id = typeof hostId === 'string' ? hostId : hostId.value
    loading.value = true
    try {
      const api = useApi()
      data.value = await api.get<SparklineData>(`/hosts/${id}/sparkline`, { range })
    } finally {
      loading.value = false
    }
  }

  return { data, loading, fetch: fetchSparkline }
}
