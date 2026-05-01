import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface EventRow {
  id: number
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
  received_at?: string
  metadata?: Record<string, any>
}

export const useEventsStore = defineStore('events', () => {
  const events = ref<EventRow[]>([])
  const loading = ref(false)
  const loaded = ref(false)
  const api = useApi()

  async function load(limit = 100) {
    loading.value = true
    try {
      events.value = await api.get<EventRow[]>('/events', { limit })
      loaded.value = true
    } finally {
      loading.value = false
    }
  }

  async function fetchOne(id: number): Promise<EventRow | null> {
    try {
      const event = await api.get<EventRow>(`/events/${id}`)
      const idx = events.value.findIndex(e => e.id === id)
      if (idx >= 0) events.value[idx] = event
      else events.value.unshift(event)
      return event
    } catch {
      return null
    }
  }

  function prepend(event: any) {
    events.value.unshift({
      id: event.id ?? -1,
      kind: event.kind,
      host_id: event.host_id,
      container_name: event.container_name,
      image: event.image,
      summary: event.summary,
      occurred_at: event.occurred_at,
    })
    if (events.value.length > 500) events.value.length = 500
  }

  return { events, loading, loaded, load, fetchOne, prepend }
})
