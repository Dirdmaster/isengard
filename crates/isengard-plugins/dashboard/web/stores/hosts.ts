import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface Host {
  id: string
  fingerprint: string
  hostname: string
  enrolled_at: string
  last_seen_at: string | null
  agent_version: string | null
  os?: string | null
  arch?: string | null
  docker_version?: string | null
}

export const useHostsStore = defineStore('hosts', () => {
  const hosts = ref<Host[]>([])
  const loading = ref(false)
  const loaded = ref(false)
  const api = useApi()

  async function load() {
    loading.value = true
    try {
      hosts.value = await api.get<Host[]>('/hosts')
      loaded.value = true
    } finally {
      loading.value = false
    }
  }

  return { hosts, loading, loaded, load }
})
