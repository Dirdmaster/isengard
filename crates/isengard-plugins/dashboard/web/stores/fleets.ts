// kill-fleets (0.15): the fleet concept has been removed end-to-end.
// This store remains as a no-op stub so dashboard components that still
// import it during the migration do not crash. New code should not use
// it; reach for host labels via `useHostsStore()` and the agent_labels
// API instead.

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
  const loaded = ref(true) // start "loaded" so spinners do not stall

  async function load() {
    fleets.value = []
    loaded.value = true
  }

  function setActive(_name: string) {
    // No-op. There is no fleet to make active.
  }

  async function create(_name: string) {
    // No-op. The /fleets endpoint is gone.
  }

  async function remove(_name: string) {
    // No-op.
  }

  return { fleets, active, loaded, load, setActive, create, remove }
})
