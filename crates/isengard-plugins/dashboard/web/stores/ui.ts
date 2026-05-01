import { defineStore } from 'pinia'
import { ref } from 'vue'

export type CmdPanePosition = 'center' | 'dock'
export type CmdPaneMode = 'navigator' | 'terminal'

export interface CmdTerminalContext {
  serviceId: string
  serviceName: string
  hostHostname: string
  fleet: string
  stackName?: string
}

export const useUiStore = defineStore('ui', () => {
  const selectedEventId = ref<number | null>(null)
  const cmdPaneOpen = ref(false)
  const cmdPanePosition = ref<CmdPanePosition>('center')
  const cmdPaneMode = ref<CmdPaneMode>('navigator')
  const cmdPaneTerminal = ref<CmdTerminalContext | null>(null)
  const activeFleet = ref<string>('all')
  const helpOpen = ref(false)

  function selectEvent(id: number | null) {
    selectedEventId.value = id
  }

  function openCmdPane(mode: CmdPaneMode = 'navigator') {
    cmdPaneMode.value = mode
    cmdPaneOpen.value = true
  }

  function closeCmdPane() {
    cmdPaneOpen.value = false
    cmdPaneMode.value = 'navigator'
    cmdPaneTerminal.value = null
  }

  function toggleCmdPanePosition() {
    cmdPanePosition.value = cmdPanePosition.value === 'center' ? 'dock' : 'center'
  }

  function openTerminalFor(svc: CmdTerminalContext) {
    cmdPaneMode.value = 'terminal'
    cmdPanePosition.value = 'dock'
    cmdPaneTerminal.value = svc
    cmdPaneOpen.value = true
  }

  function setActiveFleet(name: string) {
    activeFleet.value = name
  }

  return {
    selectedEventId,
    cmdPaneOpen,
    cmdPanePosition,
    cmdPaneMode,
    cmdPaneTerminal,
    activeFleet,
    helpOpen,
    selectEvent,
    openCmdPane,
    closeCmdPane,
    toggleCmdPanePosition,
    openTerminalFor,
    setActiveFleet,
  }
})
