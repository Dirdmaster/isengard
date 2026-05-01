import { defineStore } from 'pinia'

export interface Stack {
  id: string
  host_id: string
  name: string
  source: 'compose' | 'manual' | 'inferred'
  discovered_at: string
}

export const useStacksStore = defineStore('stacks', {
  state: () => ({
    items: [] as Stack[],
    loaded: false,
    loading: false,
    error: null as string | null,
  }),

  getters: {
    byHost: (state) => (hostId: string): Stack[] =>
      state.items.filter((s) => s.host_id === hostId),

    byId: (state) => (id: string): Stack | undefined =>
      state.items.find((s) => s.id === id),
  },

  actions: {
    async fetchAll(filters: { fleet?: string; host_id?: string } = {}) {
      this.loading = true
      this.error = null
      try {
        const api = useApi()
        const query: Record<string, string> = {}
        if (filters.fleet) query.fleet = filters.fleet
        if (filters.host_id) query.host_id = filters.host_id
        this.items = await api.get<Stack[]>('/stacks', query)
        this.loaded = true
      } catch (e) {
        this.error = e instanceof Error ? e.message : String(e)
      } finally {
        this.loading = false
      }
    },

    async fetchOne(id: string): Promise<Stack | null> {
      try {
        const api = useApi()
        const stack = await api.get<Stack>(`/stacks/${id}`)
        const idx = this.items.findIndex((s) => s.id === id)
        if (idx >= 0) this.items[idx] = stack
        else this.items.push(stack)
        return stack
      } catch {
        return null
      }
    },
  },
})
