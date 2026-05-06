import { onBeforeUnmount, onMounted } from 'vue'
import type { Ref } from 'vue'
import type { ApprovalDto } from '~/composables/useApprovals'

/**
 * Polled count of `state=open` pending approvals, shared across every
 * consumer via Nuxt's `useState` so the nav badge does not refetch on every
 * route change. A single global poller (30s) drives every subscriber.
 *
 * Usage:
 *
 * ```ts
 * const { count, error } = usePendingApprovalsCount()
 * ```
 *
 * The poller starts on first subscriber mount and stops when no subscribers
 * remain. This keeps the request volume to one /approvals fetch every 30s
 * while the dashboard is open.
 */

const POLL_INTERVAL_MS = 30_000

interface PendingApprovalsState {
  count: Ref<number>
  error: Ref<string | null>
  /** Number of currently-mounted subscribers; used to stop the poller. */
  subscribers: Ref<number>
  /** Last successful fetch timestamp (ms epoch); 0 when never fetched. */
  lastFetched: Ref<number>
}

let intervalId: ReturnType<typeof setInterval> | null = null

function ensureState(): PendingApprovalsState {
  return {
    count: useState<number>('isengard.pendingApprovals.count', () => 0),
    error: useState<string | null>(
      'isengard.pendingApprovals.error',
      () => null,
    ),
    subscribers: useState<number>(
      'isengard.pendingApprovals.subscribers',
      () => 0,
    ),
    lastFetched: useState<number>(
      'isengard.pendingApprovals.lastFetched',
      () => 0,
    ),
  }
}

async function fetchOnce(state: PendingApprovalsState) {
  const api = useApi()
  try {
    const rows = await api.get<ApprovalDto[]>('/approvals', { state: 'open' })
    state.count.value = Array.isArray(rows) ? rows.length : 0
    state.error.value = null
    state.lastFetched.value = Date.now()
  } catch (e) {
    state.error.value = e instanceof Error ? e.message : String(e)
  }
}

function startPolling(state: PendingApprovalsState) {
  if (intervalId !== null) return
  intervalId = setInterval(() => {
    if (typeof document !== 'undefined' && document.hidden) return
    fetchOnce(state).catch(() => {})
  }, POLL_INTERVAL_MS)
}

function stopPolling() {
  if (intervalId !== null) {
    clearInterval(intervalId)
    intervalId = null
  }
}

export function usePendingApprovalsCount() {
  const state = ensureState()

  onMounted(() => {
    state.subscribers.value += 1
    // Refresh immediately for the freshest badge value, but only if we have
    // no recent data. Subsequent subscribers piggyback on the running poller.
    const stale = Date.now() - state.lastFetched.value > POLL_INTERVAL_MS
    if (state.lastFetched.value === 0 || stale) {
      fetchOnce(state).catch(() => {})
    }
    startPolling(state)
  })

  onBeforeUnmount(() => {
    state.subscribers.value = Math.max(0, state.subscribers.value - 1)
    if (state.subscribers.value === 0) {
      stopPolling()
    }
  })

  /** Manual one-shot refresh; used by the page after a decide() call. */
  async function refresh() {
    await fetchOnce(state)
  }

  return {
    count: state.count,
    error: state.error,
    refresh,
  }
}
