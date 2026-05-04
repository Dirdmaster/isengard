//! Pingora server bootstrap.
//!
//! `run` is the production entrypoint (binds the HTTP and HTTPS listeners).
//! `run_for_test` binds only the HTTP listener on a caller-supplied port —
//! used by integration tests.
//!
//! Pingora 0.8 supports programmatic shutdown via `RunArgs::shutdown_signal:
//! Box<dyn ShutdownSignalWatch>`. We implement a custom watcher that resolves
//! when a tokio oneshot fires, so tests can cleanly stop the server when
//! their assertions are done. This avoids both the test-process hang we hit
//! on 0.4 (where `run_forever` called `std::process::exit`) and the SIGINT
//! workaround alternatives.

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
/// Holding it across an await would deadlock — keep it that way.
struct OneshotShutdown {
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
                // Already consumed — park forever.
                std::future::pending::<()>().await;
            }
        }
        ShutdownSignal::FastShutdown
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
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
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

pub async fn run(state: ProxyState, http_port: u16, _https_port: u16) {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state));
    svc.add_tcp(&format!("0.0.0.0:{http_port}"));
    // HTTPS listener wired in Plan B (Phase 8e). For now `https_port` is
    // accepted but unbound.
    server.add_service(svc);

    let _ = tokio::task::spawn_blocking(move || server.run(RunArgs::default())).await;
}
