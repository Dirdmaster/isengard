import { useEventListener } from '@vueuse/core'

export function useShortcuts() {
  const ui = useUiStore()

  useEventListener(window, 'keydown', (e: KeyboardEvent) => {
    const meta = e.metaKey || e.ctrlKey
    if (meta && e.key === 'k') {
      e.preventDefault()
      ui.openCmdPane('navigator')
      return
    }
    if (meta && e.key === '.') {
      e.preventDefault()
      ui.toggleCmdPanePosition()
      return
    }
    if (e.key === 'Escape' && ui.cmdPaneOpen) {
      ui.closeCmdPane()
    }
  })
}
