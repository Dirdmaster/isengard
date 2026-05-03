//! Pingora server bootstrap.
//!
//! `run` is the production entrypoint (binds the HTTP and HTTPS listeners).
//! `run_for_test` binds only the HTTP listener on a caller-supplied port —
//! used by integration tests.
//!
//! Pingora 0.4 caveat: `Server::run_forever` blocks the calling thread and
//! ultimately calls `std::process::exit(0)` to shut down. There is no
//! programmatic in-process shutdown API. The optional `shutdown_rx` parameter
//! on `run_for_test` lets the caller race the proxy task with a cancellation
//! signal, but the underlying Pingora thread will continue until the test
//! process exits. Tests that care about clean teardown should therefore run
//! the proxy in a process-per-test wrapper, or accept that the spawn_blocking
//! thread is leaked for the duration of the test process.

use crate::proxy::ProxyState;
use crate::proxy::router::IsengardProxy;
use pingora_core::server::Server;
use pingora_core::server::configuration::ServerConf;
use pingora_proxy::http_proxy_service;
use tokio::sync::oneshot;

/// Run the proxy on a single HTTP port (test-only entrypoint).
///
/// `shutdown_rx`, if `Some`, lets the caller cancel the *await* on the
/// blocking pingora thread — but does not actually stop the pingora server
/// itself (see module-level note). This is enough for integration tests that
/// only need to release the await so the test future can complete.
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

    // Pingora's `run_forever` blocks the current thread; spawn it in a
    // blocking task so tokio can keep handling the test runtime.
    let pingora_fut = tokio::task::spawn_blocking(move || server.run_forever());

    match shutdown_rx {
        Some(rx) => {
            tokio::select! {
                _ = pingora_fut => {}
                _ = rx => {
                    // Caller wants out. We can't actually stop pingora — best
                    // we can do is return and let the test process exit.
                }
            }
        }
        None => {
            let _ = pingora_fut.await;
        }
    }
}

pub async fn run(state: ProxyState, http_port: u16, _https_port: u16) {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state));
    svc.add_tcp(&format!("0.0.0.0:{http_port}"));
    // HTTPS listener wired in Plan B (Phase 8e). For now `https_port` is
    // accepted but unbound.
    server.add_service(svc);

    tokio::task::spawn_blocking(move || server.run_forever())
        .await
        .ok();
}
