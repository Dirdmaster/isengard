import { computed, ref } from 'vue'

/**
 * Mirror of `dashboard::policies::PolicyDto` (camelCase serialization).
 * See `crates/isengard-plugins/dashboard/src/policies.rs`.
 *
 * The inner `body` keeps its Rust snake_case field names because the
 * `Policy` struct in `isengard-core` has no `rename_all` attribute.
 */
export type PolicyScopeType = 'global' | 'fleet' | 'stack' | 'service' | 'container'

export type UpdateStrategy = 'pinned' | 'tag-only' | 'minor' | 'any'
export type UpdateGate = 'auto' | 'approval' | 'never'
export type FailureHandling = 'rollback' | 'keep' | 'notify'

/**
 * Mirror of `crates/isengard-core/src/policy/mod.rs::MaintenanceWindow`.
 * Phase 9d. `timezone` is optional; `null` / missing resolves to UTC at the
 * controller. Standard 5-field cron syntax (`min hour dom mon dow`).
 */
export interface MaintenanceWindow {
  cron_expr: string
  timezone?: string
}

export interface PolicyBody {
  strategy?: UpdateStrategy
  gate?: UpdateGate
  paused_until?: string
  on_failure?: FailureHandling
  approver_channel?: string
  window?: MaintenanceWindow
}

export interface PolicyDto {
  id: number
  scopeType: PolicyScopeType
  scopeKey: string
  body: PolicyBody
  createdAt: string
  updatedAt: string
}

/** URL-path sentinel used by the controller for the empty global scope_key. */
export const GLOBAL_SCOPE_URL_SENTINEL = '_'

/** Specificity rank: smaller wins, so Global is overridden by every other scope. */
const SCOPE_RANK: Record<PolicyScopeType, number> = {
  global: 0,
  fleet: 1,
  stack: 2,
  service: 3,
  container: 4,
}

/**
 * Returns the URL-safe scope_key for axum's `{*scope_key}` capture. The
 * controller treats `_` as the empty string so the global row is addressable
 * without an empty path segment.
 */
export function scopeKeyForUrl(scopeType: PolicyScopeType, scopeKey: string): string {
  if (scopeType === 'global' || scopeKey === '') return GLOBAL_SCOPE_URL_SENTINEL
  return scopeKey
}

/**
 * SWR-style policies composable. Mirrors the shape of `useEnrollment`:
 * caller drives lifecycle with `refresh()` from `onMounted`.
 *
 * Surfaces the four CRUD verbs the Settings to Policies page needs plus the
 * narrow `clearPaused` helper used by the Resume button on a paused row.
 */
export function usePolicies() {
  const api = useApi()
  const policies = ref<PolicyDto[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  const sorted = computed(() =>
    [...policies.value].sort((a, b) => {
      const ra = SCOPE_RANK[a.scopeType] ?? 99
      const rb = SCOPE_RANK[b.scopeType] ?? 99
      if (ra !== rb) return ra - rb
      return a.scopeKey.localeCompare(b.scopeKey)
    }),
  )

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      policies.value = await api.get<PolicyDto[]>('/policies')
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function removePolicy(scopeType: PolicyScopeType, scopeKey: string) {
    const key = scopeKeyForUrl(scopeType, scopeKey)
    await api.delete(`/policies/${scopeType}/${key}`)
    await refresh()
  }

  /**
   * Clears `paused_until` on a row by re-issuing a PUT with the rest of the
   * body intact. The Resume button on a paused PolicyRow calls this.
   *
   * If the row is not in the local cache (stale state), refresh first.
   */
  async function clearPaused(scopeType: PolicyScopeType, scopeKey: string) {
    let row = policies.value.find(
      p => p.scopeType === scopeType && p.scopeKey === scopeKey,
    )
    if (!row) {
      await refresh()
      row = policies.value.find(
        p => p.scopeType === scopeType && p.scopeKey === scopeKey,
      )
    }
    if (!row) {
      throw new Error(`policy (${scopeType}, ${scopeKey}) not found`)
    }
    const nextBody: PolicyBody = { ...row.body }
    delete nextBody.paused_until
    const key = scopeKeyForUrl(scopeType, scopeKey)
    await api.put(`/policies/${scopeType}/${key}`, { body: nextBody })
    await refresh()
  }

  return {
    policies,
    sorted,
    loading,
    error,
    refresh,
    removePolicy,
    clearPaused,
  }
}
