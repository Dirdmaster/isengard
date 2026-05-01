import { toast as sonnerToast } from 'vue-sonner'

export function useToast() {
  return {
    success: (text: string) => sonnerToast.success(text),
    error:   (text: string) => sonnerToast.error(text),
    info:    (text: string) => sonnerToast.info(text),
  }
}
