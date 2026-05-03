//! Upstream registry — tracks the docker container endpoints the proxy can
//! forward to. Populated by the docker watcher (Task 10) and consulted by the
//! `ProxyHttp` router on each request.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

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
}

/// Identity-only equality so the registry comparison doesn't trigger on
/// per-tick state changes (`healthy`, `consecutive_failures`).
impl PartialEq for Upstream {
    fn eq(&self, other: &Self) -> bool {
        self.container_id == other.container_id && self.addr == other.addr
    }
}
impl Eq for Upstream {}

/// Hostname-keyed registry of upstreams.
///
/// Wrapped in `Arc<RwLock<_>>` at the `ProxyState` level so the docker watcher
/// can mutate while the proxy hot-path reads. The router looks up entries by
/// the request's `Host` header (Task 9) — a later task adds SNI-based lookup
/// for the HTTPS listener.
#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    map: HashMap<String, Upstream>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the upstream for a hostname. Resets the failure
    /// streak so a freshly-registered upstream starts clean.
    pub fn set(&mut self, host: impl Into<String>, mut upstream: Upstream) {
        upstream.consecutive_failures = 0;
        self.map.insert(host.into(), upstream);
    }

    /// Apply per-rule healthcheck config to an existing upstream. No-op if
    /// the hostname isn't in the registry yet.
    pub fn set_health_config(&mut self, hostname: &str, path: Option<String>, interval: Duration) {
        if let Some(u) = self.map.get_mut(hostname) {
            u.health_path = path;
            u.health_interval = interval;
        }
    }

    pub fn get(&self, host: &str) -> Option<&Upstream> {
        self.map.get(host)
    }

    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Upstream> {
        self.map.get_mut(hostname)
    }

    pub fn remove(&mut self, host: &str) -> Option<Upstream> {
        self.map.remove(host)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Upstream)> {
        self.map.iter()
    }
}
