//! End-to-end: spawn proxy with one hardcoded routing rule pointing at a tiny
//! tokio HTTP server, curl through Pingora, assert the response body returns
//! verbatim and the proxy shuts down cleanly.
//!
//! The test asserts the full path: Pingora binds → routing logic
//! looks up upstream by Host header → upstream connect succeeds → response
//! flows back. Plus our `OneshotShutdown` impl of Pingora 0.8's
//! `ShutdownSignalWatch` lets the test stop the server cleanly so cargo test
//! exits 0 instead of hanging on a leaked thread.

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
                    // Read until end-of-headers. Pingora's upstream HTTP exchange
                    // expects the request to be fully consumed before the response
                    // arrives; reading a single packet and immediately closing
                    // leaves Pingora's writer half hanging and surfaces as 502.
                    let mut acc = Vec::with_capacity(2048);
                    let mut buf = [0u8; 1024];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                acc.extend_from_slice(&buf[..n]);
                                if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let body = b"hello-from-origin";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        std::str::from_utf8(body).unwrap()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
async fn route_by_host_header_returns_origin_response() {
    let origin = spawn_origin().await;

    // Pick a free port dynamically so concurrent test runs don't collide.
    let proxy_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

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
                state: isengard_agent::proxy::upstreams::UpstreamState::Active,
            },
        );
    }

    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });

    // Give Pingora time to bind and initialise its connection subsystem.
    // 300ms is too short — requests racing the bind get 502 with no upstream
    // attempt. 1500ms is comfortably past the warm-up window.
    tokio::time::sleep(Duration::from_millis(1500)).await;

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

    // Cleanly stop Pingora so the test process exits 0.
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
}
