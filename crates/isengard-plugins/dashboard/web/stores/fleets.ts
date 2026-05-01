import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface Fleet {
  name: string
  host_count: number
  created_at?: string
}

export const useFleetsStore = defineStore('fleets', () => {
  const fleets = ref<Fleet[]>([])
  const active = ref<string | null>(null)
  const loaded = ref(false)
  const api = useApi()

  async function load() {
    fleets.value = await api.get<Fleet[]>('/fleets')
    loaded.value = true
    if (active.value === null && fleets.value.length > 0) {
      active.value = 'all'
    }
  }

  function setActive(name: string) {
    active.value = name
  }

  async function create(name: string) {
    await api.post('/fleets', { name })
    await load()
  }

  async function remove(name: string) {
    await api.delete(`/fleets/${name}`)
    fleets.value = fleets.value.filter((f) => f.name !== name)
  }

  return { fleets, active, loaded, load, setActive, create, remove }
})
