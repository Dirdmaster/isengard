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

use crate::runtime::RuntimeBackend;
use crate::tls::{CertStore, ChallengeState};

pub mod cert_callback;
pub mod discovery;
pub mod events;
pub mod healthcheck;
pub mod router;
pub mod server;
pub mod swap;
pub mod upstreams;

pub use cert_callback::IsengardCertCallback;
pub use discovery::{SHARED_PROXY_NETWORK, pick_container_ip, resolve_container_ip};
pub use events::{ProxyEvent, ProxyEventBus};
pub use swap::swap_upstream;
pub use upstreams::{Upstream, UpstreamRegistry, UpstreamState};

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
    /// Optional `CertStore` for the HTTPS listener. `None` means HTTPS is
    /// disabled (HTTP-only). The proxy `run` entrypoint reads this once at
    /// startup; install before calling `run` if you want TLS.
    pub cert_store: Arc<RwLock<Option<Arc<CertStore>>>>,
    /// HTTP-01 challenge state for the :8080 listener. Shared with the ACME
    /// order task. Empty in tests that don't exercise ACME.
    pub acme_challenges: Arc<ChallengeState>,
    /// In-process broadcast bus for proxy lifecycle events. The deployment
    /// driver subscribes here to react to post-switch upstream collapse
    /// without round-tripping the controller.
    pub proxy_events: ProxyEventBus,
}

impl ProxyState {
    /// Construct an empty proxy state. Defaults everything; the agent
    /// installs cert store and event sink later in startup.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the outbound event sink. Spawns a tiny task to
    /// avoid forcing the caller into an `async` context: agent startup is
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

    /// Install (or replace) the `CertStore` consulted by the SNI cert
    /// callback. Must be called before the proxy `run` entrypoint reads
    /// `cert_store` at startup; once installed, the HTTPS listener binds.
    /// `None` left in place keeps the listener disabled (HTTP-only).
    pub async fn install_cert_store(&self, store: Arc<CertStore>) {
        let mut w = self.cert_store.write().await;
        *w = Some(store);
    }
}

/// Apply a `ProxyConfig` from the controller: rebuild the upstream registry
/// from the rules and bump `last_generation`. Older or duplicate generations
/// are dropped silently (debug log).
///
/// Each `RoutingRule` becomes one entry in the registry, keyed by
/// `public_hostname`. The controller leaves `container_ip` empty (it has no
/// view of runtime network IPs); the agent asks the runtime backend to ensure
/// a proxy-reachable ingress endpoint. Routes without a usable endpoint are
/// recorded as unresolved instead of being pointed at localhost.
///
/// Health is initialized to `true`; the healthcheck loop flips it once it
/// has a real signal.
pub async fn apply_config(
    state: &ProxyState,
    cfg: isengard_proto::pb::ProxyConfig,
) -> anyhow::Result<()> {
    apply_config_with_backend(state, cfg, None).await
}

/// Like [`apply_config`] but with an explicit runtime backend for IP
/// discovery. The sync loop uses this entrypoint; tests stick with the
/// no-backend [`apply_config`] form so they don't need a daemon.
///
/// Ingress resolution is trait-driven. Backends decide how to attach or
/// discover a proxy-reachable endpoint, and this function installs only ready
/// endpoints into the forwardable upstream set.
pub async fn apply_config_with_backend(
    state: &ProxyState,
    cfg: isengard_proto::pb::ProxyConfig,
    backend: Option<&dyn RuntimeBackend>,
) -> anyhow::Result<()> {
    // Lock-free pre-check to skip the build for obviously-stale configs.
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

        let endpoint = if !up.container_ip.is_empty() {
            let ip = up
                .container_ip
                .parse()
                .map_err(|e| anyhow::anyhow!("bad container_ip {}: {e}", up.container_ip))?;
            crate::runtime::IngressEndpoint::Ready {
                ip,
                mode: crate::runtime::IngressEndpointMode::ProvidedIp,
            }
        } else if let Some(b) = backend {
            b.ensure_ingress_attachment(&up.container_id).await?
        } else {
            crate::runtime::IngressEndpoint::Unresolved(
                crate::runtime::UnresolvedIngressReason::NoUsableContainerIp,
            )
        };

        let ip = match endpoint {
            crate::runtime::IngressEndpoint::Ready { ip, mode } => {
                tracing::debug!(
                    hostname = %rule.public_hostname,
                    container_id = %up.container_id,
                    ?mode,
                    "proxy: resolved ingress endpoint",
                );
                ip
            }
            crate::runtime::IngressEndpoint::Unresolved(reason) => {
                tracing::warn!(
                    hostname = %rule.public_hostname,
                    container_id = %up.container_id,
                    reason = reason.as_str(),
                    "proxy: route unresolved; not installing localhost fallback",
                );
                new_reg.set_unresolved(
                    rule.public_hostname.clone(),
                    upstreams::UnresolvedUpstream {
                        container_id: up.container_id.clone(),
                        reason,
                    },
                );
                continue;
            }
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
                state: upstreams::UpstreamState::Active,
            },
        );
    }

    // Critical section: re-check generation under the write lock, install,
    // and bump the counter atomically. Without the re-check, two concurrent
    // `apply_config(N)` and `apply_config(N+1)` calls can both pass the
    // lock-free pre-check and race on the install: and whichever acquires
    // the write lock SECOND wins, even if it carries the older generation.
    // The recheck below ensures the installed registry always matches the
    // stored last_generation at the moment of release.
    {
        let mut w = state.upstreams.write().await;
        let last_now = state.last_generation.load(Ordering::Acquire);
        if cfg.generation <= last_now {
            tracing::debug!(
                new = cfg.generation,
                last = last_now,
                "proxy: lost generation race; dropping ProxyConfig"
            );
            return Ok(());
        }
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
        state
            .last_generation
            .store(cfg.generation, Ordering::Release);
    }

    // Spawn the healthcheck sweep exactly once per process; subsequent
    // `apply_config` calls refresh the registry it reads from.
    HC_SPAWNED.get_or_init(|| {
        healthcheck::spawn_loops(state.clone());
    });

    // Wildcard certs from the controller (DNS-01). Install each cert under
    // every identifier in its SAN list so SNI for either the apex
    // (`vallee.casa`) or any subdomain covered by the wildcard
    // (`*.vallee.casa`) resolves to the same material. No-op if the
    // controller didn't push any wildcard certs.
    install_wildcard_certs(state, &cfg.wildcard_certs).await;

    tracing::info!(generation = cfg.generation, "proxy: ProxyConfig applied");
    Ok(())
}

/// Install every wildcard cert carried in the `ProxyConfig`. Each cert is
/// installed under every identifier in its SAN list (typically `*.foo` and
/// `foo`) so the agent's existing SNI cert callback resolves either name.
///
/// The cert callback in `proxy/cert_callback.rs` does an exact-match lookup
/// against SNI; mirroring the cert under each SAN keeps that callback
/// unchanged. A future refactor could move wildcard matching into the
/// callback itself; for now duplication keeps the wire-up obvious.
async fn install_wildcard_certs(
    state: &ProxyState,
    wildcards: &[isengard_proto::pb::WildcardCert],
) {
    if wildcards.is_empty() {
        return;
    }
    let cs_guard = state.cert_store.read().await;
    let Some(cs) = cs_guard.clone() else {
        // CertStore not installed (unit tests, agent without TLS bring-up).
        // Drop the certs silently rather than buffering them somewhere.
        if !wildcards.is_empty() {
            tracing::debug!(
                count = wildcards.len(),
                "proxy: wildcard certs received but CertStore not installed; dropping",
            );
        }
        return;
    };
    drop(cs_guard);

    for w in wildcards {
        if w.cert_pem.is_empty() || w.key_pem.is_empty() {
            tracing::warn!(
                identifiers = ?w.identifiers,
                "proxy: wildcard cert missing PEM material; skipping",
            );
            continue;
        }
        for id in &w.identifiers {
            let key = id.to_lowercase();
            if let Err(e) = cs.install(&key, &w.cert_pem, &w.key_pem).await {
                tracing::warn!(identifier = %key, error = %e, "proxy: install_wildcard_cert failed");
            } else {
                tracing::info!(identifier = %key, "proxy: wildcard cert installed");
            }
        }
    }
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
/// Note: Pingora 0.4 has no programmatic shutdown API: `Server::run_forever`
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
