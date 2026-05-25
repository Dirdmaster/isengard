//! Upstream registry: tracks the docker container endpoints the proxy can
//! forward to. Populated by the docker watcher (Task 10) and consulted by the
//! `ProxyHttp` router on each request.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

/// Lifecycle state for an upstream entry.
///
/// `Active` is the default: the router forwards traffic normally.
/// `Draining` marks an upstream that's being swapped out (e.g. blue/green or
/// healthcheck-driven eviction): existing in-flight requests should still
/// complete, but the next reconcile pass calls `remove_if_draining` to drop
/// the entry once it's safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    /// Router forwards traffic normally.
    Active,
    /// Swap is in progress; in-flight requests drain, no new dispatch.
    Draining,
}

/// A single proxy backend: the container that exposes the application port.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Docker container id (long form). Stable identifier across the agent.
    pub container_id: String,
    /// Address the proxy should connect to (typically the bridge IP + the
    /// container's published port).
    pub addr: SocketAddr,
    /// Last known health state. The healthcheck loop (Task 19) flips this;
    /// the router skips unhealthy entries.
    pub healthy: bool,
    /// Optional HTTP path for health probes. `None` → TCP-only liveness.
    pub health_path: Option<String>,
    /// How often the healthcheck loop probes this upstream.
    pub health_interval: Duration,
    /// Streak of consecutive failed probes. Reset on success. Used by the
    /// eviction state machine: 3 in a row → `healthy = false`.
    pub consecutive_failures: u32,
    /// Lifecycle state. New upstreams default to `Active`; the swap path
    /// flips them to `Draining` before `remove_if_draining` evicts them.
    pub state: UpstreamState,
}

/// Identity-only equality so the registry comparison doesn't trigger on
/// per-tick state changes (`healthy`, `consecutive_failures`).
impl PartialEq for Upstream {
    fn eq(&self, other: &Self) -> bool {
        self.container_id == other.container_id && self.addr == other.addr
    }
}
impl Eq for Upstream {}

/// A route whose container exists but cannot currently be reached by the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedUpstream {
    /// Docker container id (long form). Stable identifier across the agent.
    pub container_id: String,
    /// Why the agent could not resolve a proxy-reachable upstream address.
    pub reason: crate::runtime::UnresolvedIngressReason,
}

/// The current routing target for a hostname.
#[derive(Debug, Clone)]
pub enum RouteTarget {
    /// Traffic can be forwarded to a concrete upstream address.
    Ready(Upstream),
    /// The route is known but cannot be forwarded yet.
    Unresolved(UnresolvedUpstream),
}

/// Hostname-keyed registry of upstreams.
///
/// Wrapped in `Arc<RwLock<_>>` at the `ProxyState` level so the docker watcher
/// can mutate while the proxy hot-path reads. The router looks up entries by
/// the request's `Host` header (Task 9): a later task adds SNI-based lookup
/// for the HTTPS listener.
#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    /// `map` field.
    map: HashMap<String, RouteTarget>,
}

impl UpstreamRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the upstream for a hostname. Resets the failure
    /// streak so a freshly-registered upstream starts clean.
    pub fn set(&mut self, host: impl Into<String>, mut upstream: Upstream) {
        upstream.consecutive_failures = 0;
        self.map.insert(host.into(), RouteTarget::Ready(upstream));
    }

    /// Insert or replace an unresolved route for a hostname.
    pub fn set_unresolved(&mut self, host: impl Into<String>, unresolved: UnresolvedUpstream) {
        self.map
            .insert(host.into(), RouteTarget::Unresolved(unresolved));
    }

    /// Apply per-rule healthcheck config to an existing upstream. No-op if
    /// the hostname isn't in the registry yet.
    pub fn set_health_config(&mut self, hostname: &str, path: Option<String>, interval: Duration) {
        if let Some(RouteTarget::Ready(u)) = self.map.get_mut(hostname) {
            u.health_path = path;
            u.health_interval = interval;
        }
    }

    /// Look up the full route target registered for `host`.
    pub fn get_target(&self, host: &str) -> Option<&RouteTarget> {
        self.map.get(host)
    }

    /// Look up the upstream registered for `host`.
    pub fn get(&self, host: &str) -> Option<&Upstream> {
        match self.map.get(host) {
            Some(RouteTarget::Ready(upstream)) => Some(upstream),
            Some(RouteTarget::Unresolved(_)) | None => None,
        }
    }

    /// Mutable lookup. Used by the healthcheck loop to flip `healthy`.
    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Upstream> {
        match self.map.get_mut(hostname) {
            Some(RouteTarget::Ready(upstream)) => Some(upstream),
            Some(RouteTarget::Unresolved(_)) | None => None,
        }
    }

    /// Drop the entry for `host` and return it.
    pub fn remove(&mut self, host: &str) -> Option<Upstream> {
        match self.map.remove(host) {
            Some(RouteTarget::Ready(upstream)) => Some(upstream),
            Some(RouteTarget::Unresolved(_)) | None => None,
        }
    }

    /// Update the lifecycle state for an existing upstream. No-op if the
    /// hostname isn't in the registry.
    pub fn set_state(&mut self, hostname: &str, state: UpstreamState) {
        if let Some(RouteTarget::Ready(u)) = self.map.get_mut(hostname) {
            u.state = state;
        }
    }

    /// Drop an upstream only if it's currently in the `Draining` state.
    /// Active entries are left alone: used by the swap reconcile pass to
    /// finalise an in-progress drain.
    pub fn remove_if_draining(&mut self, hostname: &str) {
        if let Some(RouteTarget::Ready(u)) = self.map.get(hostname) {
            if u.state == UpstreamState::Draining {
                self.map.remove(hostname);
            }
        }
    }

    /// Iterate `(hostname, upstream)` pairs in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Upstream)> {
        self.map.iter().filter_map(|(host, target)| match target {
            RouteTarget::Ready(upstream) => Some((host, upstream)),
            RouteTarget::Unresolved(_) => None,
        })
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn sample() -> Upstream {
        Upstream {
            container_id: "c".into(),
            addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            healthy: true,
            health_path: None,
            health_interval: Duration::from_secs(10),
            consecutive_failures: 0,
            state: UpstreamState::Active,
        }
    }

    #[test]
    fn set_state_changes_state_field() {
        let mut reg = UpstreamRegistry::new();
        reg.set("h.test", sample());
        reg.set_state("h.test", UpstreamState::Draining);
        assert_eq!(reg.get("h.test").unwrap().state, UpstreamState::Draining);
    }

    #[test]
    fn remove_if_draining_removes_only_draining_entries() {
        let mut reg = UpstreamRegistry::new();
        reg.set("a.test", sample());
        reg.set("b.test", sample());
        reg.set_state("b.test", UpstreamState::Draining);

        reg.remove_if_draining("a.test"); // active → no-op
        reg.remove_if_draining("b.test"); // draining → removed
        assert!(reg.get("a.test").is_some(), "active should remain");
        assert!(reg.get("b.test").is_none(), "draining should be removed");
    }

    #[test]
    fn upstream_default_state_is_active() {
        let u = sample();
        assert_eq!(u.state, UpstreamState::Active);
    }

    #[test]
    fn unresolved_route_is_stored_without_ready_upstream() {
        let mut reg = UpstreamRegistry::new();
        reg.set_unresolved(
            "plex.test",
            UnresolvedUpstream {
                container_id: "plex".into(),
                reason: crate::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone,
            },
        );

        assert!(reg.get("plex.test").is_none());
        match reg.get_target("plex.test").unwrap() {
            RouteTarget::Unresolved(u) => {
                assert_eq!(u.container_id, "plex");
                assert_eq!(
                    u.reason,
                    crate::runtime::UnresolvedIngressReason::UnsupportedNetworkModeNone
                );
            }
            RouteTarget::Ready(_) => panic!("expected unresolved route"),
        }
    }
}
