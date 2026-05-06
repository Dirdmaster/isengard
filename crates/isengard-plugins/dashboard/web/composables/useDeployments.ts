import { computed, onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

/**
 * Mirror of `dashboard::deployments::DeploymentDto`.
 * See `crates/isengard-plugins/dashboard/src/deployments.rs`.
 */
export interface DeploymentDto {
  id: string
  host_id: string
  stack_id: number
  service_name: string
  strategy: string
  state: string
  blue_container: string | null
  green_container: string | null
  blue_digest: string
  green_digest: string
  public_hostname: string | null
  healthcheck_passed_at: string | null
  switched_at: string | null
  drained_at: string | null
  finished_at: string | null
  error: string | null
  created_at: string
  updated_at: string
  /**
   * Phase 10c (refs #50): set when this deployment is part of a multi-host
   * rolling group. The Rust DTO doesn't expose it yet but the History tab
   * checks for it defensively, so this is `optional` here.
   */
  group_id?: string | null
}

/**
 * Per-stack deployment view. Loads `active` + recent `history` on mount and
 * refreshes whenever a `deployment.*` event lands on `/ws/events` for the
 * watched stack. The WS event carries the full `Deployment` row in
 * `metadata.deployment`, so we can cheaply filter out unrelated stacks
 * before re-fetching.
 */
export function useDeployments(stackId: Ref<string> | string) {
  const api = useApi()
  const active = ref<DeploymentDto[]>([])
  const history = ref<DeploymentDto[]>([])
  const loading = ref(false)

  const sid = computed(() => {
    const raw = typeof stackId === 'string' ? stackId : stackId.value
    return raw
  })

  async function refresh() {
    const idStr = sid.value
    if (!idStr) return
    const numeric = Number(idStr)
    if (!Number.isFinite(numeric)) return

    loading.value = true
    try {
      const [a, h] = await Promise.all([
        api.get<DeploymentDto[]>('/deployments', { stack_id: numeric, state: 'active' }),
        api.get<DeploymentDto[]>('/deployments', { stack_id: numeric, state: 'history', limit: 10 }),
      ])
      active.value = a
      history.value = h
    } finally {
      loading.value = false
    }
  }

  // ---- WebSocket subscription ----------------------------------------------
  let socket: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let reconnectAttempt = 0
  let stopped = false

  function connect() {
    if (stopped) return
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    socket = new WebSocket(`${proto}//${window.location.host}/ws/events`)

    socket.addEventListener('open', () => {
      reconnectAttempt = 0
    })

    socket.addEventListener('message', (msg) => {
      try {
        const frame = JSON.parse(msg.data)
        if (frame.type !== 'event') return
        const ev = frame.event
        if (typeof ev?.kind !== 'string') return
        if (!ev.kind.startsWith('deployment.')) return

        // Only refresh when the event belongs to *this* stack. The driver
        // embeds the full row in `metadata.deployment` so we can match
        // without an extra round-trip.
        const evStackId = ev?.metadata?.deployment?.stack_id
        const numeric = Number(sid.value)
        if (typeof evStackId === 'number' && evStackId !== numeric) return

        refresh()
      } catch {
        /* malformed frame: ignore */
      }
    })

    socket.addEventListener('close', () => {
      if (stopped) return
      if (reconnectAttempt > 5) return
      const delay = Math.min(1000 * Math.pow(2, reconnectAttempt), 30000)
      reconnectAttempt++
      reconnectTimer = setTimeout(connect, delay)
    })
  }

  onMounted(() => {
    refresh()
    connect()
  })

  onBeforeUnmount(() => {
    stopped = true
    if (reconnectTimer) clearTimeout(reconnectTimer)
    if (socket) socket.close()
  })

  return { active, history, loading, refresh }
}
