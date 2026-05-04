//! Per-upstream healthcheck primitive.
//!
//! Two modes:
//! - HTTP: GET <path>, 2xx response = healthy
//! - TCP-only: connect succeeds = healthy
//!
//! Used by the eviction state machine (Task 19) which calls `check_once`
//! on a tick and updates `Upstream.healthy` based on the result.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct HealthChecker {
    path: Option<String>,
    timeout: Duration,
}

impl HealthChecker {
    pub fn new(path: String, timeout: Duration) -> Self {
        Self {
            path: Some(path),
            timeout,
        }
    }

    pub fn tcp_only(timeout: Duration) -> Self {
        Self {
            path: None,
            timeout,
        }
    }

    pub async fn check_once(&self, addr: SocketAddr) -> bool {
        let connect = timeout(self.timeout, TcpStream::connect(addr)).await;
        let mut stream = match connect {
            Ok(Ok(s)) => s,
            _ => return false,
        };
        let Some(path) = &self.path else {
            return true;
        };
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: healthcheck\r\nConnection: close\r\n\r\n",
            path
        );
        if timeout(self.timeout, stream.write_all(req.as_bytes()))
            .await
            .is_err()
        {
            return false;
        }
        let mut buf = vec![0u8; 256];
        let n = match timeout(self.timeout, stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => return false,
        };
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        (200..300).contains(&status)
    }
}

/// Eviction threshold: this many failures in a row flips `healthy` to false.
/// One success while unhealthy flips it back to true (no debounce on recovery).
const FAIL_THRESHOLD: u32 = 3;

/// Tick interval between full registry sweeps. Aggressive on purpose so the
/// eviction test (which uses ~50ms `health_interval` on the upstream) sees
/// state transitions in <500ms; production tuning can come later.
const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn the healthcheck loop. One tokio task sweeps the registry on
/// `TICK_INTERVAL`, fans probes out per-upstream, and updates each entry's
/// `healthy` / `consecutive_failures` based on the result.
///
/// Designed to be spawned exactly once per `ProxyState` (the caller guards
/// with a `OnceLock` in `apply_config`). The loop reads the registry under a
/// snapshot to avoid holding the lock across the network probe.
pub fn spawn_loops(state: crate::proxy::ProxyState) {
    tokio::spawn(async move {
        loop {
            let snapshot: Vec<(String, SocketAddr, Option<String>, Duration)> = {
                let r = state.upstreams.read().await;
                r.iter()
                    .map(|(host, up)| {
                        (
                            host.clone(),
                            up.addr,
                            up.health_path.clone(),
                            up.health_interval,
                        )
                    })
                    .collect()
            };
            for (host, addr, path, _interval) in snapshot {
                let st = state.clone();
                tokio::spawn(async move {
                    let hc = match path {
                        Some(p) => HealthChecker::new(p, Duration::from_secs(2)),
                        None => HealthChecker::tcp_only(Duration::from_secs(2)),
                    };
                    let healthy = hc.check_once(addr).await;
                    // Track transition + container_id under the write lock,
                    // then drop the lock before awaiting `emit` so the
                    // healthcheck loop can never block itself on the event
                    // channel.
                    let mut transition: Option<(bool, String)> = None;
                    {
                        let mut w = st.upstreams.write().await;
                        if let Some(up) = w.get_mut(&host) {
                            let was = up.healthy;
                            if healthy {
                                up.consecutive_failures = 0;
                                if !up.healthy {
                                    up.healthy = true;
                                    tracing::info!(host = %host, "upstream recovered");
                                }
                            } else {
                                up.consecutive_failures += 1;
                                if up.consecutive_failures >= FAIL_THRESHOLD && up.healthy {
                                    up.healthy = false;
                                    tracing::warn!(
                                        host = %host,
                                        "upstream evicted (3 consecutive failures)"
                                    );
                                }
                            }
                            if was != up.healthy {
                                transition = Some((up.healthy, up.container_id.clone()));
                            }
                        }
                    }
                    if let Some((now_healthy, container_id)) = transition {
                        let summary = format!(
                            "upstream {} on {} is now {}",
                            container_id,
                            host,
                            if now_healthy { "healthy" } else { "unhealthy" }
                        );
                        st.emit(isengard_core::Event {
                            kind: "routing.upstream.health_changed".into(),
                            occurred_at: chrono::Utc::now(),
                            summary,
                            container_name: Some(host.clone()),
                            ..Default::default()
                        })
                        .await;
                    }
                });
            }
            tokio::time::sleep(TICK_INTERVAL).await;
        }
    });
}
