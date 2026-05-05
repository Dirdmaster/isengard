import { ref } from 'vue'

/**
 * Mirror of `dashboard::enrollment::TokenListEntry`.
 * See `crates/isengard-plugins/dashboard/src/enrollment.rs`.
 */
export interface ActiveToken {
  hash_prefix: string
  role: string
  expires_at: string
  created_at: string
}

/**
 * Mirror of `dashboard::enrollment::MintTokenResponse`.
 * The plaintext `token` is shown to the operator exactly once.
 */
export interface MintedToken {
  token: string
  expires_at: string
}

/**
 * Enrollment-token + per-host cert revocation API surface for the dashboard.
 *
 * Exposes the four endpoints from Phase 14 Task 13:
 *   - GET    /enrollment/tokens                  (list active tokens)
 *   - POST   /enrollment/tokens                  (mint, returns plaintext once)
 *   - DELETE /enrollment/tokens/:hash_prefix     (revoke unconsumed token)
 *   - DELETE /hosts/:host_id/cert                (revoke a host's leaf cert)
 *
 * The composable does not auto-load on creation: callers are expected to
 * `refresh()` from `onMounted` so each instance controls its own lifecycle.
 */
export function useEnrollment() {
  const api = useApi()
  const tokens = ref<ActiveToken[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function refresh() {
    loading.value = true
    error.value = null
    try {
      tokens.value = await api.get<ActiveToken[]>('/enrollment/tokens')
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function mint(role: 'agent', ttlSeconds: number): Promise<MintedToken> {
    return await api.post<MintedToken>('/enrollment/tokens', {
      role,
      ttl_seconds: ttlSeconds,
    })
  }

  async function revokeToken(hashPrefix: string) {
    await api.delete(`/enrollment/tokens/${hashPrefix}`)
    await refresh()
  }

  async function revokeHostCert(hostId: string) {
    await api.delete(`/hosts/${hostId}/cert`)
  }

  return { tokens, loading, error, refresh, mint, revokeToken, revokeHostCert }
}
