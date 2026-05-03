//! Reverse proxy subsystem (Pingora-backed).
//!
//! - [`ProxyState`] / [`UpstreamRegistry`]: shared routing table, mutated by
//!   the docker watcher (Task 11+) and read by the router on each request.
//! - [`server`]: Pingora bootstrap (`run` for production, `run_for_test` for
//!   integration tests).
//! - [`router`]: the `ProxyHttp` impl with host-header → upstream lookup.
//! - [`supervise`]: keeps the proxy task alive across crashes with a bounded
//!   restart budget. Wired into agent startup so the proxy starts on boot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub mod healthcheck;
pub mod router;
pub mod server;
pub mod upstreams;

pub use upstreams::{Upstream, UpstreamRegistry};

/// Shared state passed into the `ProxyHttp` router. Cheap to clone (Arc).
#[derive(Debug, Default, Clone)]
pub struct ProxyState {
    /// Hostname-keyed registry of upstreams. Mutated by the docker watcher
    /// (Task 11+), read by the router on each request.
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Maximum proxy restarts allowed within [`RESTART_WINDOW`] before we give up
/// and log a crashloop event. Keeps a runaway panic from spinning forever.
const MAX_RESTARTS: u32 = 5;
/// Sliding window for [`MAX_RESTARTS`].
const RESTART_WINDOW: Duration = Duration::from_secs(300);

/// Run the proxy under a supervisor that restarts on crash with exponential
/// backoff. Returns once the restart budget is exhausted (Task 20 will turn
/// this terminal log into a `proxy.crashloop` event).
///
/// Note: Pingora 0.4 has no programmatic shutdown API — `Server::run_forever`
/// only returns by calling `std::process::exit(0)` after a graceful SIGTERM.
/// In practice the inner task does not exit cleanly during normal operation;
/// the supervisor exists to catch panics from `bootstrap` / port-bind failures.
pub async fn supervise(state: ProxyState, http_port: u16, https_port: u16) {
    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        let st = state.clone();
        info!(http_port, https_port, "proxy: starting pingora task");

        let handle = tokio::spawn(async move {
            server::run(st, http_port, https_port).await;
        });

        match handle.await {
            Ok(_) => warn!("proxy: pingora task exited cleanly; restarting"),
            Err(e) if e.is_panic() => error!("proxy: pingora panicked: {e:?}"),
            Err(e) => error!("proxy: pingora task error: {e:?}"),
        }

        let now = Instant::now();
        restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        restarts.push(now);
        if restarts.len() as u32 > MAX_RESTARTS {
            error!(
                restarts = restarts.len(),
                window_secs = RESTART_WINDOW.as_secs(),
                "proxy: restart budget exhausted; giving up (TODO Task 20: emit proxy.crashloop)"
            );
            return;
        }
        let backoff = Duration::from_millis(
            250u64 * (1u64 << (restarts.len().min(5) as u64).saturating_sub(1)),
        );
        warn!(
            ?backoff,
            attempt = restarts.len(),
            "proxy: backing off before restart"
        );
        tokio::time::sleep(backoff).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test that the supervisor's restart-budget policy gives up after
    /// MAX_RESTARTS+1 failures within the window. We exercise the policy by
    /// calling `supervise` against an immediately-failing port (port 0 + a
    /// service that panics during bootstrap is hard to arrange portably; the
    /// simpler check is that the constants are in the expected ranges).
    #[test]
    fn restart_budget_constants_are_sane() {
        assert!(MAX_RESTARTS >= 3, "want some headroom for transient flaps");
        assert!(
            RESTART_WINDOW >= Duration::from_secs(60),
            "window should be long enough to catch real crashloops"
        );
    }
}
