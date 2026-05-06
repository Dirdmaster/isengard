import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

/**
 * Mirror of `dashboard::approvals::ApprovalDto` (camelCase serialization).
 * See `crates/isengard-plugins/dashboard/src/approvals.rs`.
 *
 * State is one of: `pending_open`, `pending_approved`, `pending_rejected`,
 * `pending_expired`, `pending_snoozed`. Wire JSON values are snake_case
 * because the Rust enum uses `#[serde(rename_all = "snake_case")]`.
 */
export type ApprovalState =
  | 'pending_open'
  | 'pending_approved'
  | 'pending_rejected'
  | 'pending_expired'
  | 'pending_snoozed'

export interface ApprovalDto {
  actionId: string
  state: ApprovalState
  hostId: string
  stack: string
  service: string
  containerName: string
  image: string
  currentDigest: string
  proposedDigest: string
  diffUrl: string | null
  approverChannel: string | null
  expiresAt: string
  decidedAt: string | null
  decidedBy: string | null
  metadata: Record<string, unknown> | null
  createdAt: string
  updatedAt: string
}

export type ApprovalFilterState = 'open' | 'decided' | 'all'

export type DecisionKind = 'approve' | 'reject' | 'snooze'

export interface DecisionResponseDto {
  approval: ApprovalDto
  dispatchedApplyUpdate: boolean
  pausedUntilSet: string | null
}

const REFRESH_INTERVAL_MS = 60_000

/**
 * Approvals queue composable. Caller drives lifecycle from `onMounted`
 * via `refresh()`. Auto-refreshes every 60s while the page is visible.
 *
 * The filter is a writable ref; flipping it triggers a refresh.
 */
export function useApprovals(initialFilter: ApprovalFilterState = 'open') {
  const api = useApi()

  const approvals = ref<ApprovalDto[]>([])
  const filter = ref<ApprovalFilterState>(initialFilter)
  const loading = ref(false)
  const error = ref<string | null>(null)
  const inflightDecisions = ref<Set<string>>(new Set())

  /** Newest-first (created_at DESC): backend already orders this way. */
  const sorted = computed<ApprovalDto[]>(() => approvals.value)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      const rows = await api.get<ApprovalDto[]>('/approvals', {
        state: filter.value,
      })
      approvals.value = rows
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function setFilter(next: ApprovalFilterState) {
    if (filter.value === next) return
    filter.value = next
    await refresh()
  }

  /**
   * Optimistic decide: drops the row from the list immediately when the
   * filter is `open` (it's leaving the queue), then refreshes on success.
   * On failure, restores the row and re-throws so the caller can toast.
   */
  async function decide(
    actionId: string,
    decision: DecisionKind,
    snoozeHours?: number,
  ): Promise<DecisionResponseDto> {
    if (inflightDecisions.value.has(actionId)) {
      throw new Error('decision already in flight for this approval')
    }
    inflightDecisions.value.add(actionId)

    const removeOptimistically = filter.value === 'open'
    const previousIndex = approvals.value.findIndex(a => a.actionId === actionId)
    const previousRow = previousIndex >= 0 ? approvals.value[previousIndex] : null

    if (removeOptimistically && previousIndex >= 0) {
      approvals.value = approvals.value.filter(a => a.actionId !== actionId)
    }

    const body: Record<string, unknown> = { decision }
    if (decision === 'snooze' && typeof snoozeHours === 'number') {
      body.snoozeHours = snoozeHours
    }

    try {
      const resp = await api.post<DecisionResponseDto>(
        `/approvals/${actionId}`,
        body,
      )
      // Refresh from the server so derived rows (decided_by, decided_at,
      // updated state) are accurate.
      await refresh().catch(() => {
        // Refresh failures are non-fatal; the optimistic update stands.
      })
      return resp
    } catch (e) {
      // Rollback: re-insert the row if we removed it optimistically.
      if (removeOptimistically && previousRow && previousIndex >= 0) {
        const restored = [...approvals.value]
        restored.splice(
          Math.min(previousIndex, restored.length),
          0,
          previousRow,
        )
        approvals.value = restored
      }
      throw e
    } finally {
      inflightDecisions.value.delete(actionId)
    }
  }

  function isInFlight(actionId: string): boolean {
    return inflightDecisions.value.has(actionId)
  }

  // Auto-refresh while the page is visible. Pause when the document is
  // hidden so we don't waste fetches on a backgrounded tab.
  let intervalId: ReturnType<typeof setInterval> | null = null
  let visibilityHandler: (() => void) | null = null

  function startInterval() {
    if (intervalId !== null) return
    intervalId = setInterval(() => {
      if (typeof document !== 'undefined' && document.hidden) return
      refresh().catch(() => {
        // Errors surface via the `error` ref; swallow here to avoid
        // unhandled-rejection noise.
      })
    }, REFRESH_INTERVAL_MS)
  }

  function stopInterval() {
    if (intervalId !== null) {
      clearInterval(intervalId)
      intervalId = null
    }
  }

  onMounted(() => {
    startInterval()
    if (typeof document !== 'undefined') {
      visibilityHandler = () => {
        if (!document.hidden) {
          refresh().catch(() => {})
        }
      }
      document.addEventListener('visibilitychange', visibilityHandler)
    }
  })

  onBeforeUnmount(() => {
    stopInterval()
    if (visibilityHandler && typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', visibilityHandler)
      visibilityHandler = null
    }
  })

  return {
    approvals,
    sorted,
    filter,
    loading,
    error,
    refresh,
    setFilter,
    decide,
    isInFlight,
  }
}
