//! Pingora server bootstrap.
//!
//! `run` is the production entrypoint (binds the HTTP and HTTPS listeners).
//! `run_for_test` binds only the HTTP listener on a caller-supplied port:
//! used by integration tests.
//!
//! Pingora 0.8 supports programmatic shutdown via `RunArgs::shutdown_signal:
//! Box<dyn ShutdownSignalWatch>`. We implement a custom watcher that resolves
//! when a tokio oneshot fires, so tests can cleanly stop the server when
//! their assertions are done. This avoids both the test-process hang we hit
//! on 0.4 (where `run_forever` called `std::process::exit`) and the SIGINT
//! workaround alternatives.
//!
//! Shutdown timing (Pingora 0.8 source, server/mod.rs `run` + `main_loop`):
//!   - SIGTERM arrives. The main loop broadcasts shutdown to every
//!     `run_endpoint` accept-loop task. Each task selects against its
//!     `accept()` future and breaks out of the loop. The `TransportStack`
//!     drops at end of scope, which drops the `Arc<Listener>`; once all
//!     clones are dropped, the underlying `TcpListener` closes and the
//!     port returns to the free pool. In practice this happens within
//!     milliseconds.
//!   - The main loop then `thread::sleep(grace_period_seconds)`. The
//!     DEFAULT is `EXIT_TIMEOUT` = 300 seconds (5 minutes). During this
//!     window the listener has been released but the process is still
//!     alive holding tokio runtime state for in-flight connections.
//!   - After the grace, the runtime shuts down with
//!     `graceful_shutdown_timeout_seconds` (default 5s).
//!
//! Our tune: 10s grace, 30s runtime shutdown. The agent's only
//! long-lived connections are operator-proxied HTTP requests; 10s is
//! more than enough for any reasonable request to finish. systemd's
//! TimeoutStopSec=15s on the unit will SIGKILL the process if it
//! over-runs the cycle anyway.

use crate::proxy::ProxyState;
use crate::proxy::router::IsengardProxy;
use async_trait::async_trait;
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora_proxy::http_proxy_service;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// `ShutdownSignalWatch` impl backed by a tokio oneshot. Pingora's `run`
/// will call `recv()` and shut down when our test sends on the channel.
///
/// Uses `std::sync::Mutex` (NOT `tokio::sync::Mutex`): the lock is only held
/// briefly to do `.take()` on the inner Option, never across `.await`.
/// Holding it across an await would deadlock: keep it that way.
struct OneshotShutdown {
    /// `rx` field.
    rx: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl ShutdownSignalWatch for OneshotShutdown {
    async fn recv(&self) -> ShutdownSignal {
        let taken = self.rx.lock().unwrap().take();
        match taken {
            Some(rx) => {
                let _ = rx.await;
            }
            None => {
                // Already consumed: park forever.
                std::future::pending::<()>().await;
            }
        }
        ShutdownSignal::FastShutdown
    }
}

/// Build a [`ServerConf`] with the project's tuned shutdown windows.
///
/// Pingora's defaults (300s grace, 5s runtime shutdown) were the
/// proximate cause of the `isengard update` bind failure on lausanne
/// v0.5.2: the old process held cleanup state for 5 minutes after
/// SIGTERM, which interacted badly with `systemctl restart`'s "start
/// the new ExecStart immediately" semantics.
///
/// The new windows:
///   - `grace_period_seconds = 10`. After the listener drops we give
///     in-flight requests 10s to finish. The proxy only
///     handles HTTP requests for stack workloads; 10s is the upper
///     bound on any sensible request.
///   - `graceful_shutdown_timeout_seconds = 30`. Tokio runtime
///     shutdown window. 30s is the cap before systemd's
///     TimeoutStopSec=15s (set on isd-agent.service in this commit)
///     escalates to SIGKILL, so the SIGKILL is the actual ceiling.
fn server_conf() -> ServerConf {
    ServerConf {
        grace_period_seconds: Some(10),
        graceful_shutdown_timeout_seconds: Some(30),
        ..ServerConf::default()
    }
}

/// Run the proxy on a single HTTP port (test-only entrypoint).
///
/// If `shutdown_rx` is `Some`, signalling on it triggers Pingora's
/// graceful-shutdown path and `run_for_test` returns cleanly.
pub async fn run_for_test(
    state: ProxyState,
    port: u16,
    shutdown_rx: Option<oneshot::Receiver<()>>,
) {
    let mut server = Server::new_with_opt_and_conf(None, server_conf());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state));
    svc.add_tcp(&format!("127.0.0.1:{port}"));
    server.add_service(svc);

    // RunArgs holds a `Box<dyn ShutdownSignalWatch>` which isn't Send, so it
    // can't be captured by the spawn_blocking closure. Construct it inside.
    let _ = tokio::task::spawn_blocking(move || {
        let run_args = match shutdown_rx {
            Some(rx) => RunArgs {
                shutdown_signal: Box::new(OneshotShutdown {
                    rx: Mutex::new(Some(rx)),
                }),
            },
            None => RunArgs::default(),
        };
        server.run(run_args);
    })
    .await;
}

/// Bind the Pingora listeners and run them until the process exits. The
/// HTTPS listener is registered only when a cert store has been installed
/// onto `state`.
pub async fn run(state: ProxyState, http_port: u16, https_port: u16) {
    let cert_store_opt = state.cert_store.read().await.clone();

    let mut server = Server::new_with_opt_and_conf(None, server_conf());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state.clone()));
    svc.add_tcp(&format!("0.0.0.0:{http_port}"));

    if let Some(cert_store) = cert_store_opt {
        use pingora_core::listeners::tls::TlsSettings;
        let callback = Box::new(crate::proxy::IsengardCertCallback::new(cert_store));
        match TlsSettings::with_callbacks(callback) {
            Ok(tls_settings) => {
                svc.add_tls_with_settings(&format!("0.0.0.0:{https_port}"), None, tls_settings);
                tracing::info!(
                    http_port,
                    https_port,
                    "proxy: HTTPS listener configured (boringssl)"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "proxy: failed to build TlsSettings; HTTPS disabled");
            }
        }
    } else {
        tracing::info!(
            http_port,
            "proxy: HTTPS listener disabled (no cert_store installed)"
        );
    }

    server.add_service(svc);
    let _ = tokio::task::spawn_blocking(move || server.run(RunArgs::default())).await;
}
