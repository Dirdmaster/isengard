import { ref, type Ref } from 'vue'

/**
 * Phase 13B wire protocol. Server -> client frames are JSON text. See
 * `docs/superpowers/specs/2026-05-06-phase-13b-logs-streaming-design.md`.
 */
export interface LogLine {
  /** Frame timestamp (RFC3339). Empty for control frames. */
  ts: string
  /** "stdout" or "stderr". Empty for control frames. */
  stream: 'stdout' | 'stderr' | ''
  /** Decoded line body, no trailing newline. */
  msg: string
  /** Short host id the line came from. "agg" when aggregated. */
  host: string
  /** True for backfill frames so the UI can dim or label them. */
  backfill?: boolean
}

export type LogStreamState =
  | 'idle'
  | 'connecting'
  | 'connected'
  | 'paused'
  | 'closed'
  | 'error'

export interface LogStreamError {
  reason: string
  host?: string
}

/** Hard cap so a noisy container can't OOM the tab. */
const MAX_LINES = 5000

export function useLogStream(
  stackId: Ref<string> | string,
  serviceName: Ref<string> | string,
) {
  const lines = ref<LogLine[]>([])
  const state = ref<LogStreamState>('idle')
  const error = ref<LogStreamError | null>(null)
  /** Per-host counter of lines dropped to backpressure since connect. */
  const dropped = ref<Record<string, number>>({})
  /** Hosts the controller actually opened streams against. */
  const hosts = ref<string[]>([])

  let ws: WebSocket | null = null
  let paused = false

  function url(): string {
    const sid = typeof stackId === 'string' ? stackId : stackId.value
    const name = typeof serviceName === 'string' ? serviceName : serviceName.value
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    return `${proto}//${window.location.host}/api/v1/services/${sid}/${encodeURIComponent(
      name,
    )}/logs/ws`
  }

  function connect() {
    if (ws) return
    state.value = 'connecting'
    error.value = null
    ws = new WebSocket(url())
    ws.onopen = () => {
      state.value = 'connected'
    }
    ws.onclose = () => {
      state.value = 'closed'
    }
    ws.onerror = () => {
      state.value = 'error'
    }
    ws.onmessage = (ev) => {
      let frame: any
      try {
        frame = JSON.parse(ev.data)
      } catch {
        return
      }
      if (paused) return
      handleFrame(frame)
    }
  }

  function handleFrame(frame: any) {
    if (!frame || typeof frame.type !== 'string') return
    switch (frame.type) {
      case 'backfill': {
        const host = frame.host ?? ''
        if (host && !hosts.value.includes(host)) hosts.value.push(host)
        const incoming: LogLine[] = (frame.lines ?? []).map((l: any) => ({
          ts: l.ts ?? '',
          stream: l.stream ?? '',
          msg: l.msg ?? '',
          host,
          backfill: true,
        }))
        push(incoming)
        break
      }
      case 'line': {
        const host = frame.host ?? ''
        if (host && !hosts.value.includes(host)) hosts.value.push(host)
        push([
          {
            ts: frame.ts ?? '',
            stream: frame.stream ?? '',
            msg: frame.msg ?? '',
            host,
          },
        ])
        break
      }
      case 'dropped': {
        const host = frame.host ?? ''
        const count = Number(frame.count ?? 0)
        dropped.value = { ...dropped.value, [host]: (dropped.value[host] ?? 0) + count }
        break
      }
      case 'unavailable': {
        error.value = { reason: String(frame.reason ?? 'unknown'), host: frame.host }
        break
      }
      case 'closed': {
        state.value = 'closed'
        break
      }
    }
  }

  function push(incoming: LogLine[]) {
    if (incoming.length === 0) return
    const next = lines.value.concat(incoming)
    if (next.length > MAX_LINES) {
      lines.value = next.slice(next.length - MAX_LINES)
    } else {
      lines.value = next
    }
  }

  function pause() {
    paused = true
    state.value = 'paused'
    send({ type: 'pause' })
  }

  function resume() {
    paused = false
    state.value = 'connected'
    send({ type: 'resume' })
  }

  function setTail(n: number) {
    send({ type: 'seek_tail', tail: n })
  }

  function setLevel(level: 'all' | 'info' | 'warn' | 'error') {
    send({ type: 'set_level', level })
  }

  function clear() {
    lines.value = []
    dropped.value = {}
    error.value = null
  }

  function send(payload: object) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    try {
      ws.send(JSON.stringify(payload))
    } catch {
      /* ignore */
    }
  }

  function disconnect() {
    if (ws) {
      try {
        ws.close()
      } catch {
        /* ignore */
      }
      ws = null
    }
    state.value = 'closed'
  }

  return {
    lines,
    state,
    error,
    dropped,
    hosts,
    connect,
    disconnect,
    pause,
    resume,
    setTail,
    setLevel,
    clear,
  }
}
