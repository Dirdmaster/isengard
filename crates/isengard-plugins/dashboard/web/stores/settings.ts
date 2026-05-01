import { defineStore } from 'pinia'
import { ref } from 'vue'

export const useSettingsStore = defineStore('settings', () => {
  const values = ref<Record<string, unknown>>({})
  const loaded = ref(false)
  const loading = ref(false)
  const api = useApi()

  async function load() {
    loading.value = true
    try {
      const data = await api.get<{ values: Record<string, unknown> }>('/settings')
      values.value = data.values
      loaded.value = true
    } finally {
      loading.value = false
    }
  }

  async function patch(updates: Record<string, unknown>) {
    await api.patch('/settings', { values: updates })
    Object.assign(values.value, updates)
  }

  return { values, loaded, loading, load, patch }
})
