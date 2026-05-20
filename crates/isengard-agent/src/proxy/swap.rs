//! Hot-swap an upstream with drain semantics.
//!
//! [`swap_upstream`] flips the existing entry to `Draining`, schedules a
//! background cleanup that runs after the grace period, then installs the new
//! upstream as `Active` under the same hostname. The router's
//! [`super::router::IsengardProxy::upstream_peer`] short-circuits requests
//! routed to a `Draining` entry with a 503: but in the swap path the new
//! entry replaces the old immediately, so traffic continues uninterrupted.
//!
//! The cleanup uses [`UpstreamRegistry::remove_if_draining`], which is a no-op
//! when the entry has been re-set to `Active` in the meantime. That makes
//! repeated swaps within the grace window safe: only the *currently draining*
//! entry is ever evicted, never the live one.

use crate::proxy::{ProxyState, Upstream, UpstreamState};
use anyhow::Result;
use std::time::Duration;

/// Replace the upstream for `hostname` with `new_upstream`. The old entry is
/// flipped to `Draining` and removed after `grace_period` (only if still
/// draining at that point), giving in-flight requests time to complete.
///
/// **Atomic from the router's view:** the Draining flip and the new-upstream
/// install happen under a single write lock, so the router never observes a
/// transient `Draining` state for the swapped hostname (which would cause a
/// 503 between the two operations). `spawn_drain_cleanup` only schedules a
/// tokio task: no lock contention while held.
pub async fn swap_upstream(
    state: &ProxyState,
    hostname: &str,
    new_upstream: Upstream,
    grace_period: Duration,
) -> Result<()> {
    let mut new_active = new_upstream;
    new_active.state = UpstreamState::Active;

    let mut w = state.upstreams.write().await;
    // Mark the old entry draining first so the cleanup task (scheduled below)
    // has the right state to compare against.
    w.set_state(hostname, UpstreamState::Draining);
    // tokio::spawn doesn't take the lock: holding the write lock across this
    // call is microsecond-cheap.
    spawn_drain_cleanup(state.clone(), hostname.to_string(), grace_period);
    // Install the new upstream under the SAME lock. The router's next read
    // sees `Active` directly: no Draining window from its perspective.
    w.set(hostname.to_string(), new_active);
    drop(w);

    Ok(())
}

/// Spawn a tokio task that, after `grace_period`, removes `hostname` from the
/// registry only if it's still in the `Draining` state. Safe to call multiple
/// times: each call schedules an independent cleanup, and `remove_if_draining`
/// is a no-op for `Active` entries.
pub fn spawn_drain_cleanup(state: ProxyState, hostname: String, grace_period: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(grace_period).await;
        let mut w = state.upstreams.write().await;
        w.remove_if_draining(&hostname);
    });
}
