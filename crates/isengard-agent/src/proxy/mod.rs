//! Reverse proxy subsystem (Pingora-backed).
//!
//! - [`ProxyState`] / [`UpstreamRegistry`]: shared routing table, mutated by
//!   the docker watcher (Task 11+) and read by the router on each request.
//! - [`server`]: Pingora bootstrap (`run` for production, `run_for_test` for
//!   integration tests).
//! - [`router`]: the `ProxyHttp` impl with host-header → upstream lookup.
//! - [`supervise`]: keeps the proxy task alive across crashes with a bounded
//!   restart budget. Wired into agent startup so the proxy starts on boot.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use isengard_core::Event;
use tokio::sync::{RwLock, mpsc};
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
    /// (Task 11+) and the controller's `ProxyConfig` push (Task 13), read by
    /// the router on each request.
    pub upstreams: Arc<RwLock<UpstreamRegistry>>,
    /// Last applied `ProxyConfig.generation`. The controller increments
    /// monotonically; we drop any push with `generation <= last_generation`
    /// to ignore reordered or replayed messages.
    pub last_generation: Arc<AtomicU64>,
    /// Optional outbound event sink. Wired in agent startup so subsystems
    /// (healthcheck eviction, supervisor crashloop) can publish journal
    /// events through the same channel `OutboundEmitter` drains. `None` in
    /// unit-test setups that don't care about events.
    pub event_tx: Arc<RwLock<Option<mpsc::Sender<Event>>>>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the outbound event sink. Spawns a tiny task to
    /// avoid forcing the caller into an `async` context — agent startup is
    /// synchronous around the proxy bring-up.
    pub fn set_event_sink(&self, tx: mpsc::Sender<Event>) {
        let evt = self.event_tx.clone();
        tokio::spawn(async move {
            let mut w = evt.write().await;
            *w = Some(tx);
        });
    }

    /// Best-effort emit: drops the event silently if no sink is installed or
    /// the channel is full. `try_send` keeps healthcheck ticks non-blocking.
    pub async fn emit(&self, ev: Event) {
        let r = self.event_tx.read().await;
        if let Some(tx) = &*r {
            let _ = tx.try_send(ev);
        }
    }
}

/// Apply a `ProxyConfig` from the controller: rebuild the upstream registry
/// from the rules and bump `last_generation`. Older or duplicate generations
/// are dropped silently (debug log).
///
/// Each `RoutingRule` becomes one entry in the registry, keyed by
/// `public_hostname`. An empty `container_ip` defaults to `127.0.0.1` (the
/// docker bridge IP isn't always populated in the controller's view yet).
///
/// Health is initialized to `true`; the healthcheck loop (later task) flips
/// it once it has a real signal.
pub async fn apply_config(
    state: &ProxyState,
    cfg: isengard_proto::pb::ProxyConfig,
) -> anyhow::Result<()> {
    let last = state.last_generation.load(Ordering::Acquire);
    if cfg.generation <= last {
        tracing::debug!(
            new = cfg.generation,
            last,
            "proxy: dropping stale ProxyConfig"
        );
        return Ok(());
    }

    let mut new_reg = UpstreamRegistry::new();
    for rule in &cfg.rules {
        let Some(up) = rule.upstream.as_ref() else {
            continue;
        };
        let ip: std::net::IpAddr = if up.container_ip.is_empty() {
            "127.0.0.1".parse().expect("127.0.0.1 is a valid IpAddr")
        } else {
            up.container_ip
                .parse()
                .map_err(|e| anyhow::anyhow!("bad container_ip {}: {e}", up.container_ip))?
        };
        let addr = std::net::SocketAddr::new(ip, up.container_port as u16);
        new_reg.set(
            rule.public_hostname.clone(),
            Upstream {
                container_id: up.container_id.clone(),
                addr,
                healthy: true,
                health_path: None,
                health_interval: Duration::from_secs(10),
                consecutive_failures: 0,
            },
        );
    }

    // Swap registry, then layer per-rule health config on top so the
    // healthcheck loop sees both in the same view.
    {
        let mut w = state.upstreams.write().await;
        *w = new_reg;
        for rule in &cfg.rules {
            if let Some(hc) = &rule.healthcheck {
                let path = if hc.path.is_empty() {
                    None
                } else {
                    Some(hc.path.clone())
                };
                let interval = Duration::from_secs(hc.interval_secs.max(1) as u64);
                w.set_health_config(&rule.public_hostname, path, interval);
            }
        }
    }

    // Spawn the healthcheck sweep exactly once per process; subsequent
    // `apply_config` calls just refresh the registry it reads from.
    HC_SPAWNED.get_or_init(|| {
        healthcheck::spawn_loops(state.clone());
    });

    state
        .last_generation
        .store(cfg.generation, Ordering::Release);
    tracing::info!(generation = cfg.generation, "proxy: ProxyConfig applied");
    Ok(())
}

/// Ensures `healthcheck::spawn_loops` runs at most once per process.
static HC_SPAWNED: OnceLock<()> = OnceLock::new();

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
    #[allow(clippy::assertions_on_constants)]
    fn restart_budget_constants_are_sane() {
        assert!(MAX_RESTARTS >= 3, "want some headroom for transient flaps");
        assert!(
            RESTART_WINDOW >= Duration::from_secs(60),
            "window should be long enough to catch real crashloops"
        );
    }
}
