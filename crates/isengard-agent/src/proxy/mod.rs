//! Reverse proxy subsystem (Pingora-backed).
//!
//! Phase 8b skeleton: `ProxyState` + `UpstreamRegistry` and a stubbed
//! `ProxyHttp` impl. Actual listener wiring lands in Task 9; the docker
//! watcher feeding the registry lands in Task 10.

use std::sync::Arc;

use tokio::sync::RwLock;

pub mod healthcheck;
pub mod router;
pub mod server;
pub mod upstreams;

pub use upstreams::{Upstream, UpstreamRegistry};

/// Shared state passed into the `ProxyHttp` router. Cheap to clone (Arc).
#[derive(Debug, Default, Clone)]
pub struct ProxyState {
    /// Hostname-keyed registry of upstreams. Mutated by the docker watcher
    /// (Task 10), read by the router on each request.
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self::default()
    }
}
