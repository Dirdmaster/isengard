import { defineStore } from 'pinia'

export interface Service {
  id: string
  host_id: string
  stack_id: string | null
  name: string
  image: string
  state: 'running' | 'stopped' | 'restarting' | 'unknown'
}

export const useServicesStore = defineStore('services', {
  state: () => ({
    items: [] as Service[],
    loaded: false,
    loading: false,
  }),

  getters: {
    byStack: (state) => (stackId: string): Service[] =>
      state.items.filter((s) => s.stack_id === stackId),
  },

  actions: {
    async fetchByStack(stackId: string) {
      this.loading = true
      try {
        const api = useApi()
        const items = await api.get<Service[]>('/services', { stack_id: stackId })
        // Replace items with same stack_id, keep others.
        this.items = this.items
          .filter((s) => s.stack_id !== stackId)
          .concat(items)
        this.loaded = true
      } finally {
        this.loading = false
      }
    },
  },
})
