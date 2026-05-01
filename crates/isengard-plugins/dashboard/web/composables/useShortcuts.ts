import { useEventListener } from '@vueuse/core'

function isInputFocused(): boolean {
  const a = document.activeElement
  return !!(a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA' || (a as HTMLElement).isContentEditable))
}

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
    if (e.key === '?' && !e.metaKey && !e.ctrlKey && !isInputFocused()) {
      e.preventDefault()
      ui.helpOpen = !ui.helpOpen
      return
    }
    if (e.key === 'Escape') {
      if (ui.helpOpen) {
        ui.helpOpen = false
        return
      }
      if (ui.cmdPaneOpen) {
        ui.closeCmdPane()
      }
    }
  })
}
