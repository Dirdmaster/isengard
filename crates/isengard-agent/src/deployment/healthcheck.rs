//! Polling/threshold/deadline wrapper around `proxy::healthcheck::HealthChecker`.
//!
//! The underlying `HealthChecker::check_once` answers a single question
//! ("is this addr healthy right now?"). Blue-green deployment needs more:
//! poll on an interval, require N consecutive passes before declaring the
//! green container healthy, and give up after a deadline. This wrapper
//! composes those semantics on top of the existing primitive.
//!
//! See spec
//! Deployment healthcheck helpers.
//! §Components → `healthcheck.rs`.

use crate::proxy::healthcheck::HealthChecker;
use chrono::{DateTime, Utc};
use std::net::SocketAddr;
use std::time::Duration;

/// One probe attempt: when it ran and whether `check_once` returned true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptResult {
    /// When the probe ran.
    pub at: DateTime<Utc>,
    /// `true` when `check_once` returned healthy.
    pub passed: bool,
}

/// Returned when `wait_for_healthy` exhausts its deadline without seeing
/// `success_threshold` consecutive passes. The last (up to 5) attempts are
/// included for diagnostics: enough to see the recent failure pattern
/// without unbounded memory growth on long deadlines.
#[derive(Debug, Clone)]
pub struct HealthcheckTimeout {
    /// Last up-to-5 attempts in chronological order.
    pub last_attempts: Vec<AttemptResult>,
}

/// Internal constant: MAX TRACKED ATTEMPTS.
const MAX_TRACKED_ATTEMPTS: usize = 5;

/// Polling wrapper. Defaults: 1s interval, 3 consecutive passes, no initial
/// delay, 60s deadline. Override with the `with_*` builder methods.
pub struct DeploymentHealthcheck {
    /// `inner` field.
    inner: HealthChecker,
    /// `interval` field.
    interval: Duration,
    /// `success_threshold` field.
    success_threshold: u32,
    /// `initial_delay` field.
    initial_delay: Duration,
    /// `deadline` field.
    deadline: Duration,
}

impl DeploymentHealthcheck {
    /// Build a wrapper around `inner` with default cadence + thresholds.
    pub fn new(inner: HealthChecker) -> Self {
        Self {
            inner,
            interval: Duration::from_secs(1),
            success_threshold: 3,
            initial_delay: Duration::ZERO,
            deadline: Duration::from_secs(60),
        }
    }

    /// Set the poll interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Set the number of consecutive passes required to declare healthy.
    pub fn with_success_threshold(mut self, success_threshold: u32) -> Self {
        self.success_threshold = success_threshold;
        self
    }

    /// Set the grace period before the first probe.
    pub fn with_initial_delay(mut self, initial_delay: Duration) -> Self {
        self.initial_delay = initial_delay;
        self
    }

    /// Set the overall deadline.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Poll `addr` on `interval`, returning the timestamp of the Nth
    /// consecutive pass once the threshold is reached. Returns
    /// `HealthcheckTimeout` if `deadline` elapses first.
    ///
    /// `deadline` is measured from the moment this function is entered,
    /// BEFORE `initial_delay` is subtracted from it. A long initial delay
    /// can therefore burn most of the deadline before any probe runs.
    pub async fn wait_for_healthy(
        &self,
        addr: SocketAddr,
    ) -> Result<DateTime<Utc>, HealthcheckTimeout> {
        let start = std::time::Instant::now();
        if !self.initial_delay.is_zero() {
            tokio::time::sleep(self.initial_delay).await;
        }
        let mut consecutive: u32 = 0;
        let mut attempts: Vec<AttemptResult> = Vec::new();
        loop {
            if start.elapsed() >= self.deadline {
                return Err(HealthcheckTimeout {
                    last_attempts: attempts,
                });
            }
            let passed = self.inner.check_once(addr).await;
            let at = Utc::now();
            attempts.push(AttemptResult { at, passed });
            if attempts.len() > MAX_TRACKED_ATTEMPTS {
                let drop_n = attempts.len() - MAX_TRACKED_ATTEMPTS;
                attempts.drain(0..drop_n);
            }
            if passed {
                consecutive += 1;
                if consecutive >= self.success_threshold {
                    return Ok(at);
                }
            } else {
                consecutive = 0;
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tokio::net::TcpListener as TokioTcpListener;

    fn live_addr_listener() -> (SocketAddr, TokioTcpListener) {
        let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = TokioTcpListener::from_std(std_listener).unwrap();
        (addr, listener)
    }

    /// A guaranteed-refused address: bind an ephemeral port, capture it,
    /// then drop the listener. Subsequent connects to the captured port
    /// will fail with ECONNREFUSED on Darwin/Linux dev machines (no
    /// reliance on whether port 1 happens to be filtered).
    fn dead_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    /// Keep accepting connections in the background so TCP probes succeed
    /// for the lifetime of the test. The returned task aborts when dropped
    /// at end of test scope.
    fn spawn_acceptor(listener: TokioTcpListener) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        })
    }

    #[tokio::test]
    async fn passes_after_threshold() {
        let (addr, listener) = live_addr_listener();
        let _accept = spawn_acceptor(listener);
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(200)))
            .with_interval(Duration::from_millis(20))
            .with_success_threshold(3)
            .with_deadline(Duration::from_secs(2));
        let res = hc.wait_for_healthy(addr).await;
        assert!(res.is_ok(), "expected healthy, got {:?}", res.err());
    }

    #[tokio::test]
    async fn deadline_timeout() {
        let addr = dead_addr();
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(50)))
            .with_interval(Duration::from_millis(20))
            .with_success_threshold(3)
            .with_deadline(Duration::from_millis(300));
        let res = hc.wait_for_healthy(addr).await;
        assert!(res.is_err(), "expected timeout");
        let to = res.err().unwrap();
        assert!(!to.last_attempts.is_empty());
        assert!(to.last_attempts.iter().all(|a| !a.passed));
    }

    #[tokio::test]
    async fn last_attempts_capped_at_five() {
        let addr = dead_addr();
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(20)))
            .with_interval(Duration::from_millis(10))
            .with_success_threshold(99)
            .with_deadline(Duration::from_millis(800));
        let res = hc.wait_for_healthy(addr).await;
        let to = res.expect_err("should time out");
        assert!(
            to.last_attempts.len() <= MAX_TRACKED_ATTEMPTS,
            "expected ≤5 attempts retained, got {}",
            to.last_attempts.len()
        );
    }

    #[tokio::test]
    async fn initial_delay_postpones_first_check() {
        let (addr, listener) = live_addr_listener();
        let _accept = spawn_acceptor(listener);
        let initial = Duration::from_millis(150);
        let hc = DeploymentHealthcheck::new(HealthChecker::tcp_only(Duration::from_millis(100)))
            .with_interval(Duration::from_millis(20))
            .with_success_threshold(1)
            .with_initial_delay(initial)
            .with_deadline(Duration::from_secs(2));
        let started = std::time::Instant::now();
        let res = hc.wait_for_healthy(addr).await;
        let elapsed = started.elapsed();
        assert!(res.is_ok());
        assert!(
            elapsed >= initial,
            "expected ≥{:?} elapsed before first probe, got {:?}",
            initial,
            elapsed
        );
    }

    #[tokio::test]
    async fn fail_resets_consecutive_counter() {
        // Scripted HTTP server: returns 200 for probes 1..=2, 500 for probes
        // 3..=4, then 200 forever. With threshold=3, success is only possible
        // AFTER the failure window IF the counter resets on every failure.
        // If reset were broken (counter persisted), we would see Ok() after
        // the very first pass-streak collapses: but in that case the run
        // would never reach 3 consecutive passes either, so the test
        // requires Ok() and asserts it took longer than 3 intervals. The
        // strict proof: minimum elapsed must include the failure window.
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener as TokioTcpListener;
        use tokio::sync::Mutex;

        let (addr, std_listener) = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.set_nonblocking(true).unwrap();
            let a = l.local_addr().unwrap();
            (a, l)
        };
        let listener = TokioTcpListener::from_std(std_listener).unwrap();
        let counter = Arc::new(Mutex::new(0u32));
        let counter_srv = counter.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let counter_srv = counter_srv.clone();
                tokio::spawn(async move {
                    let n = {
                        let mut g = counter_srv.lock().await;
                        *g += 1;
                        *g
                    };
                    let status = if (1..=2).contains(&n) || n >= 5 {
                        "200 OK"
                    } else {
                        "500 Internal Server Error"
                    };
                    // Drain the request line so the client doesn't see RST.
                    let mut buf = [0u8; 512];
                    let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                    let body = format!(
                        "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        status
                    );
                    let _ = sock.write_all(body.as_bytes()).await;
                });
            }
        });

        let interval = Duration::from_millis(60);
        let hc = DeploymentHealthcheck::new(HealthChecker::new(
            "/health".into(),
            Duration::from_millis(300),
        ))
        .with_interval(interval)
        .with_success_threshold(3)
        .with_deadline(Duration::from_secs(5));

        let started = std::time::Instant::now();
        let res = hc.wait_for_healthy(addr).await;
        let elapsed = started.elapsed();
        server.abort();
        let _ = server.await;

        assert!(res.is_ok(), "expected eventual success: {:?}", res.err());
        // Total probes needed if reset works: 2 pass + 2 fail + 3 pass = 7.
        // Minimum elapsed ≈ 6 * interval (sleeps between probes).
        let min_expected = interval * 6;
        assert!(
            elapsed >= min_expected,
            "elapsed {:?} < min {:?}: counter likely did not reset on failure",
            elapsed,
            min_expected
        );
        let final_count = *counter.lock().await;
        assert!(
            final_count >= 7,
            "expected ≥7 probes (2 pass + 2 fail + 3 pass), saw {}",
            final_count
        );
    }
}
