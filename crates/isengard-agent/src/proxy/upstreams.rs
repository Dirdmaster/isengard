//! Upstream registry — tracks the docker container endpoints the proxy can
//! forward to. Populated by the docker watcher (Task 10) and consulted by the
//! `ProxyHttp` router on each request.

use std::collections::HashMap;
use std::net::SocketAddr;

/// A single proxy backend: the container that exposes the application port.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Docker container id (long form). Stable identifier across the agent.
    pub container_id: String,
    /// Address the proxy should connect to (typically the bridge IP + the
    /// container's published port).
    pub addr: SocketAddr,
    /// Last known health state. The healthcheck loop (Task — later) flips
    /// this; the router skips unhealthy entries.
    pub healthy: bool,
}

/// Container-id keyed registry of upstreams.
///
/// Wrapped in `Arc<RwLock<_>>` at the `ProxyState` level so the docker watcher
/// can mutate while the proxy hot-path reads.
#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    map: HashMap<String, Upstream>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an upstream entry.
    pub fn set(&mut self, upstream: Upstream) {
        self.map.insert(upstream.container_id.clone(), upstream);
    }

    pub fn get(&self, container_id: &str) -> Option<&Upstream> {
        self.map.get(container_id)
    }

    pub fn remove(&mut self, container_id: &str) -> Option<Upstream> {
        self.map.remove(container_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Upstream)> {
        self.map.iter()
    }
}
