import { ref, onMounted, onBeforeUnmount } from 'vue'

export interface LiveEvent {
  kind: string
  host_id: string | null
  container_name: string | null
  image: string | null
  summary: string
  occurred_at: string
}

export function useEventStream() {
  const connected = ref(false)
  const events = ref<LiveEvent[]>([])
  let socket: WebSocket | null = null

  function connect() {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${proto}//${window.location.host}/ws/events`
    socket = new WebSocket(url)
    socket.addEventListener('open', () => { connected.value = true })
    socket.addEventListener('close', () => { connected.value = false })
    socket.addEventListener('message', (msg) => {
      try {
        const frame = JSON.parse(msg.data)
        if (frame.type === 'event') {
          events.value.unshift(frame.event)
          // Cap memory.
          if (events.value.length > 500) events.value.length = 500
        }
      } catch { /* ignore */ }
    })
  }

  onMounted(connect)
  onBeforeUnmount(() => { socket?.close() })

  return { connected, events }
}
