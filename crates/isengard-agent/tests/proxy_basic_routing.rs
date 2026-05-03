//! End-to-end: spawn proxy with one hardcoded rule pointing at a tiny
//! tokio HTTP server, curl it via Pingora, assert the response body.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_origin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let body = b"hello-from-origin";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        std::str::from_utf8(body).unwrap()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        }
    });
    addr
}

// KNOWN LIMITATION (confirmed in Task 10):
// Pingora 0.4 has no programmatic in-process shutdown API. `Server::run_forever`
// installs SIGINT/SIGTERM/SIGQUIT handlers and ultimately calls
// `std::process::exit(0)` on graceful shutdown — there is no public sender on
// the internal `ShutdownWatch` we could trigger from Rust code. Even with the
// new `shutdown_rx` parameter on `run_for_test`, the spawn_blocking thread
// running `run_forever` is leaked for the lifetime of the test process and
// makes `cargo test` hang on this binary's teardown.
// Real e2e proxy verification is planned via a process-per-test wrapper or a
// container-based fixture (Plan B). For now this test stays #[ignore]'d as
// documentation of the expected end-to-end behavior; correctness of the Task 9
// routing logic is verified by code review of `router.rs::upstream_peer`.
#[tokio::test]
#[ignore = "Pingora 0.4 has no programmatic shutdown — see comment above"]
async fn route_by_host_header_returns_origin_response() {
    let origin = spawn_origin().await;

    let proxy_port = 18080u16;
    let state = isengard_agent::proxy::ProxyState::new();
    {
        let mut up = state.upstreams.write().await;
        up.set(
            "blog.test",
            isengard_agent::proxy::Upstream {
                container_id: "test".into(),
                addr: origin,
                healthy: true,
                health_path: None,
                health_interval: Duration::from_secs(10),
                consecutive_failures: 0,
            },
        );
    }

    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });

    // give pingora a moment to bind
    tokio::time::sleep(Duration::from_millis(300)).await;

    let body = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/", proxy_port))
        .header("Host", "blog.test")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "hello-from-origin");

    // Best-effort: signal `run_for_test` to stop awaiting and let the test
    // future complete. The underlying pingora thread still leaks (see comment
    // at the top of the file), so cargo test will still hang on teardown of
    // this binary — the #[ignore] above keeps it out of the default run.
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), proxy_task).await;
}
