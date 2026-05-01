import { ref } from 'vue'

interface ConfirmOpts {
  title: string
  description: string
  confirmText?: string
  danger?: boolean
}

// Module-scoped state: single dialog at a time, shared across composable instances.
const open = ref(false)
const opts = ref<ConfirmOpts>({ title: '', description: '' })
let resolver: ((confirmed: boolean) => void) | null = null

export function useConfirm() {
  function confirm(o: ConfirmOpts): Promise<boolean> {
    opts.value = o
    open.value = true
    return new Promise((resolve) => { resolver = resolve })
  }

  function resolve(confirmed: boolean) {
    open.value = false
    resolver?.(confirmed)
    resolver = null
  }

  return { open, opts, confirm, resolve }
}
