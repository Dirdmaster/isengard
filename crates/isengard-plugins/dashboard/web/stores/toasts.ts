import { defineStore } from 'pinia'
import { ref } from 'vue'

export type ToastKind = 'success' | 'error' | 'info'

export interface Toast {
  id: number
  kind: ToastKind
  text: string
}

let nextId = 1

export const useToastsStore = defineStore('toasts', () => {
  const items = ref<Toast[]>([])

  function push(kind: ToastKind, text: string, durationMs = 4000) {
    const t: Toast = { id: nextId++, kind, text }
    items.value.push(t)
    if (items.value.length > 3) items.value.splice(0, items.value.length - 3)
    setTimeout(() => dismiss(t.id), durationMs)
  }

  function dismiss(id: number) {
    items.value = items.value.filter((t) => t.id !== id)
  }

  return { items, push, dismiss }
})
