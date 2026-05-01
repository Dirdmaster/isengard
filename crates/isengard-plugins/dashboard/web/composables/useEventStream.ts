import { ref, onMounted, onBeforeUnmount, watchEffect, type Ref } from 'vue'

export interface LiveEvent {
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
}

export type ConnectionState = 'connecting' | 'live' | 'reconnecting' | 'offline'

export function useEventStream() {
  const connectionState: Ref<ConnectionState> = ref('connecting')
  const events = ref<LiveEvent[]>([])
  let socket: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let reconnectAttempt = 0

  function connect() {
    connectionState.value = reconnectAttempt > 0 ? 'reconnecting' : 'connecting'
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    socket = new WebSocket(`${proto}//${window.location.host}/ws/events`)

    socket.addEventListener('open', () => {
      connectionState.value = 'live'
      reconnectAttempt = 0
    })

    socket.addEventListener('message', (msg) => {
      try {
        const frame = JSON.parse(msg.data)
        if (frame.type === 'event') {
          events.value.unshift(frame.event)
          if (events.value.length > 500) events.value.length = 500
          // Push into the global Pinia store so EventTimeline / StateStrip update.
          const eventsStore = useEventsStore()
          eventsStore.prepend(frame.event)
        }
      } catch { /* ignore */ }
    })

    socket.addEventListener('close', () => {
      if (reconnectAttempt > 5) {
        connectionState.value = 'offline'
        return
      }
      connectionState.value = 'reconnecting'
      const delay = Math.min(1000 * Math.pow(2, reconnectAttempt), 30000)
      reconnectAttempt++
      reconnectTimer = setTimeout(connect, delay)
    })
  }

  function disconnect() {
    if (reconnectTimer) clearTimeout(reconnectTimer)
    if (socket) socket.close()
    connectionState.value = 'offline'
  }

  // Backward-compat boolean for callers that haven't migrated to connectionState.
  const connected = ref(false)
  watchEffect(() => {
    connected.value = connectionState.value === 'live'
  })

  // Keep auto-connect behavior on mount (existing call sites depend on this)
  onMounted(connect)
  onBeforeUnmount(disconnect)

  return {
    connected,
    connectionState,
    events,
    connect,
    disconnect,
  }
}
