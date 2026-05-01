import { useToastsStore } from '~/stores/toasts'

export function useToast() {
  const store = useToastsStore()
  return {
    success: (text: string) => store.push('success', text),
    error:   (text: string) => store.push('error', text),
    info:    (text: string) => store.push('info', text),
  }
}
