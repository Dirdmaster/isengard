import { ref, watch, type Ref } from 'vue'

/**
 * Mirror of `crates/isengard-core/src/policy/resolve.rs::PolicyOrigin`.
 * Serialised as kebab-case strings.
 */
export type PolicyOrigin =
  | 'default'
  | 'global'
  | 'fleet'
  | 'stack'
  | 'service'
  | 'container'

/**
 * Mirror of `crates/isengard-core/src/policy/mod.rs::UpdateStrategy`.
 */
export type UpdateStrategy = 'pinned' | 'tag-only' | 'minor' | 'any'

/**
 * Mirror of `crates/isengard-core/src/policy/mod.rs::UpdateGate`.
 */
export type UpdateGate = 'auto' | 'approval' | 'never'

/**
 * Mirror of `crates/isengard-core/src/policy/mod.rs::FailureHandling`.
 */
export type FailureHandling = 'rollback' | 'keep' | 'notify'

export interface ResolvedProvenance {
  strategy: PolicyOrigin
  gate: PolicyOrigin
  paused_until: PolicyOrigin
  on_failure: PolicyOrigin
  approver_channel: PolicyOrigin
}

/**
 * Mirror of `crates/isengard-core/src/policy/resolve.rs::ResolvedPolicy`.
 * This is the JSON shape returned by GET /api/v1/policies/effective.
 */
export interface ResolvedPolicy {
  strategy: UpdateStrategy
  gate: UpdateGate
  paused_until: string | null
  on_failure: FailureHandling
  approver_channel: string | null
  provenance: ResolvedProvenance
}

export interface EffectivePolicyContext {
  fleet?: string
  stack?: string
  service?: string
  host_id?: string
  container?: string
}

/**
 * Lazy fetcher for `/api/v1/policies/effective`. Does NOT auto-load on mount:
 * callers explicitly invoke `load()` (typically when a collapsible expands)
 * to avoid hammering the API with one request per service row on a stack
 * detail page.
 *
 * Re-fetches when `ctx` changes after the first load. If `ctx` becomes empty
 * (every field undefined / empty), the call is skipped to avoid a useless
 * request that would return the implicit defaults regardless.
 */
export function useEffectivePolicy(ctx: Ref<EffectivePolicyContext>) {
  const effective = ref<ResolvedPolicy | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)
  let started = false

  function buildQuery(c: EffectivePolicyContext): Record<string, string> {
    const q: Record<string, string> = {}
    if (c.fleet) q.fleet = c.fleet
    if (c.stack) q.stack = c.stack
    if (c.service) q.service = c.service
    if (c.host_id) q.host_id = c.host_id
    if (c.container) q.container = c.container
    return q
  }

  async function fetchOnce() {
    const c = ctx.value
    const q = buildQuery(c)
    if (Object.keys(q).length === 0) {
      // Nothing to scope by: skip. Caller can re-trigger when ctx populates.
      return
    }
    loading.value = true
    error.value = null
    try {
      const api = useApi()
      effective.value = await api.get<ResolvedPolicy>('/policies/effective', q)
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  /**
   * First call kicks off the fetch and starts watching `ctx` for changes;
   * subsequent calls are idempotent re-fetches (useful for a Retry button).
   */
  async function load() {
    if (!started) {
      started = true
      watch(
        () => ({
          fleet: ctx.value.fleet,
          stack: ctx.value.stack,
          service: ctx.value.service,
          host_id: ctx.value.host_id,
          container: ctx.value.container,
        }),
        () => {
          fetchOnce()
        },
        { deep: false },
      )
    }
    await fetchOnce()
  }

  return { effective, loading, error, load }
}
