//! Pingora server bootstrap.
//!
//! `run` is the production entrypoint (binds the HTTP and HTTPS listeners).
//! `run_for_test` binds only the HTTP listener on a caller-supplied port —
//! used by integration tests.

use crate::proxy::ProxyState;
use crate::proxy::router::IsengardProxy;
use pingora_core::server::Server;
use pingora_core::server::configuration::ServerConf;
use pingora_proxy::http_proxy_service;

pub async fn run_for_test(state: ProxyState, port: u16) {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    server.bootstrap();

    let mut svc = http_proxy_service(&server.configuration, IsengardProxy::new(state));
    svc.add_tcp(&format!("127.0.0.1:{port}"));
    server.add_service(svc);

    // Pingora's `run_forever` blocks the current thread; spawn it in a
    // blocking task so tokio can keep handling the test runtime.
    tokio::task::spawn_blocking(move || server.run_forever())
        .await
        .ok();
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
