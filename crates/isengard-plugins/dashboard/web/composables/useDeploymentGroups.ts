import { computed, onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

/**
 * Mirror of `dashboard::deployment_groups::DeploymentGroupDto`.
 * Phase 10c (T5 refs #50).
 */
export interface DeploymentGroupDto {
  id: string
  stack_id: number
  service_name: string
  parallelism: string
  state: string
  target_hosts: string[]
  started_at: string
  finished_at: string | null
  error: string | null
}

export interface DeploymentGroupDetailDto extends DeploymentGroupDto {
  deployments: Array<{
    id: string
    host_id: string
    state: string
    error: string | null
    service_name: string
    created_at: string
    updated_at: string
  }>
}

const TERMINAL_GROUP_STATES = new Set(['done', 'aborted', 'failed'])

/**
 * Per-stack rolling-group view. Mirrors `useDeployments.ts` for behaviour:
 * SWR fetch on mount, refresh on websocket `deployment.*` frames whose
 * embedded deployment row belongs to this stack.
 *
 * Returns:
 * - `active` : groups that are pending or rolling.
 * - `latest` : most recent group regardless of state (used for the "just
 *   finished" bar collapse).
 * - `loading`: true during the first fetch only.
 */
export function useDeploymentGroups(stackId: Ref<string> | string) {
  const api = useApi()
  const groups = ref<DeploymentGroupDto[]>([])
  const loading = ref(false)

  const sid = computed(() => {
    const raw = typeof stackId === 'string' ? stackId : stackId.value
    return raw
  })

  const active = computed(() =>
    groups.value.filter((g) => !TERMINAL_GROUP_STATES.has(g.state)),
  )

  const latest = computed(() => {
    if (groups.value.length === 0) return null
    return [...groups.value].sort(
      (a, b) => new Date(b.started_at).getTime() - new Date(a.started_at).getTime(),
    )[0]
  })

  async function refresh() {
    const idStr = sid.value
    if (!idStr) return
    const numeric = Number(idStr)
    if (!Number.isFinite(numeric)) return

    loading.value = true
    try {
      groups.value = await api.get<DeploymentGroupDto[]>('/deployment-groups', {
        stack_id: numeric,
      })
    } finally {
      loading.value = false
    }
  }

  async function fetchDetail(id: string) {
    return api.get<DeploymentGroupDetailDto>(`/deployment-groups/${id}`)
  }

  async function abort(id: string) {
    return api.delete<void>(`/deployment-groups/${id}`)
  }

  // ---- WebSocket subscription --------------------------------------------
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
        const evStackId = ev?.metadata?.deployment?.stack_id
        const numeric = Number(sid.value)
        if (typeof evStackId === 'number' && evStackId !== numeric) return
        refresh()
      } catch {
        /* malformed: ignore */
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

  return { groups, active, latest, loading, refresh, fetchDetail, abort }
}

/**
 * Per-stack parallelism setting helper. Wraps the
 * `/api/v1/stacks/:id/deployment-parallelism` endpoints.
 */
export function useStackParallelism(stackId: Ref<string> | string) {
  const api = useApi()
  const value = ref<string | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)

  const sid = computed(() => {
    const raw = typeof stackId === 'string' ? stackId : stackId.value
    return raw
  })

  async function refresh() {
    const numeric = Number(sid.value)
    if (!Number.isFinite(numeric)) return
    loading.value = true
    error.value = null
    try {
      const dto = await api.get<{ stack_id: number; parallelism: string | null }>(
        `/stacks/${numeric}/deployment-parallelism`,
      )
      value.value = dto.parallelism
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  async function set(parallelism: string | null) {
    const numeric = Number(sid.value)
    if (!Number.isFinite(numeric)) return
    await api.post(`/stacks/${numeric}/deployment-parallelism`, { parallelism })
    value.value = parallelism
  }

  return { value, loading, error, refresh, set }
}
