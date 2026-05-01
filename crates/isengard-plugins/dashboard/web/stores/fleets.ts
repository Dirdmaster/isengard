import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface Fleet {
  name: string
  host_count: number
}

export const useFleetsStore = defineStore('fleets', () => {
  const fleets = ref<Fleet[]>([])
  const active = ref<string | null>(null)
  const api = useApi()

  async function load() {
    fleets.value = await api.get<Fleet[]>('/fleets')
    if (active.value === null && fleets.value.length > 0) {
      active.value = 'all'
    }
  }

  function setActive(name: string) {
    active.value = name
  }

  return { fleets, active, load, setActive }
})
