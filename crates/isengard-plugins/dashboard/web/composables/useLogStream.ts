import { ref, type Ref } from 'vue'

export interface LogLine {
  stream: 'stdout' | 'stderr'
  text: string
  ts: number
}

export type LogStreamState = 'idle' | 'connecting' | 'connected' | 'closed' | 'error'

export interface LogStreamMessage {
  type: 'info' | 'error'
  message: string
}

export function useLogStream(serviceId: Ref<string> | string) {
  const lines = ref<LogLine[]>([])
  const message = ref<LogStreamMessage | null>(null)
  const state = ref<LogStreamState>('idle')
  let ws: WebSocket | null = null

  function connect() {
    state.value = 'connecting'
    const id = typeof serviceId === 'string' ? serviceId : serviceId.value
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    ws = new WebSocket(`${proto}//${window.location.host}/api/v1/services/${id}/logs`)
    ws.onopen = () => { state.value = 'connected' }
    ws.onclose = () => { state.value = 'closed' }
    ws.onerror = () => { state.value = 'error' }
    ws.onmessage = (ev) => {
      try {
        const frame = JSON.parse(ev.data)
        if (frame.type === 'line') {
          lines.value.push({ stream: frame.stream, text: frame.text, ts: frame.ts })
        } else if (frame.type === 'info' || frame.type === 'error') {
          message.value = { type: frame.type, message: frame.message ?? frame.error ?? '' }
        }
      } catch {}
    }
  }

  function disconnect() {
    if (ws) {
      try { ws.send(JSON.stringify({ type: 'close' })) } catch {}
      ws.close()
      ws = null
    }
    state.value = 'closed'
  }

  return { lines, message, state, connect, disconnect }
}
