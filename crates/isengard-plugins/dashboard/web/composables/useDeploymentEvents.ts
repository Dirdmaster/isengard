import { ref, watch, type Ref } from 'vue'

/**
 * Mirror of `dashboard::dto::EventDto` (the subset the row-expand timeline
 * needs). The full record carries more fields than this; we destructure on
 * the read path so adding new ones in Rust doesn't break the front-end.
 *
 * Phase 10c (T4 refs #50).
 */
export interface DeploymentTimelineEvent {
  id: number
  kind: string
  summary: string
  error: string | null
  occurred_at: string
  metadata: Record<string, any>
}

/**
 * Fetch every event whose `metadata.deployment.id` matches `deploymentId`.
 *
 * Behaviour:
 * - Returns a reactive `ref` that updates whenever `deploymentId` changes.
 * - Empty / undefined `deploymentId` clears the list and skips the request.
 * - Errors are swallowed into `error` (consumers render an inline note).
 *
 * The backend extends `/api/v1/events?deployment_id=` to do the filtering;
 * see `crates/isengard-plugins/dashboard/src/api.rs::list_events`.
 */
export function useDeploymentEvents(deploymentId: Ref<string | null>) {
  const api = useApi()
  const events = ref<DeploymentTimelineEvent[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    const id = deploymentId.value
    if (!id) {
      events.value = []
      return
    }
    loading.value = true
    error.value = null
    try {
      const rows = await api.get<DeploymentTimelineEvent[]>('/events', {
        deployment_id: id,
        limit: 200,
      })
      // The backend returns newest first; the timeline reads chronologically.
      events.value = rows.slice().sort((a, b) => {
        return new Date(a.occurred_at).getTime() - new Date(b.occurred_at).getTime()
      })
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      events.value = []
    } finally {
      loading.value = false
    }
  }

  watch(deploymentId, refresh, { immediate: true })

  return { events, loading, error, refresh }
}
