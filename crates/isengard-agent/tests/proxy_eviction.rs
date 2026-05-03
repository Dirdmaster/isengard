//! Task 19: healthcheck loop + eviction state machine.
//!
//! End-to-end: spawn a tiny HTTP origin whose 200/503 response is toggled by
//! a `watch::Receiver`, wire it into a `ProxyState`, run the healthcheck
//! loop, and assert that the upstream flips to `healthy = false` after 3
//! consecutive failures and back to `healthy = true` on the next success.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

use isengard_agent::proxy::{ProxyState, Upstream};

async fn spawn(toggle: tokio::sync::watch::Receiver<bool>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                let healthy = *toggle.borrow();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b).await;
                let resp = if healthy {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                } else {
                    "HTTP/1.1 503 SU\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = s.write_all(resp.as_bytes()).await;
            }
        }
    });
    addr
}

#[tokio::test]
async fn eviction_after_three_failures_then_recovery_after_one_success() {
    let (tx, rx) = tokio::sync::watch::channel(true);
    let addr = spawn(rx).await;

    let state = ProxyState::new();
    {
        let mut up = state.upstreams.write().await;
        up.set(
            "h.test",
            Upstream {
                container_id: "c".into(),
                addr,
                healthy: true,
                health_path: None,
                health_interval: Duration::from_millis(50),
                consecutive_failures: 0,
            },
        );
        up.set_health_config("h.test", Some("/healthz".into()), Duration::from_millis(50));
    }

    isengard_agent::proxy::healthcheck::spawn_loops(state.clone());

    // Flip to unhealthy, wait for >=3 checks, confirm evicted. The loop
    // ticks every 50ms and the threshold is 3, so 400ms gives ~8 checks of
    // headroom for scheduling jitter on loaded CI.
    tx.send(false).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    {
        let up = state.upstreams.read().await;
        let entry = up.get("h.test").unwrap();
        assert!(
            !entry.healthy,
            "should be evicted after 3 failures (failures={})",
            entry.consecutive_failures
        );
        assert!(entry.consecutive_failures >= 3);
    }

    // Flip back to healthy, wait for at least one successful tick.
    tx.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let up = state.upstreams.read().await;
        let entry = up.get("h.test").unwrap();
        assert!(entry.healthy, "should be healthy again after one success");
        assert_eq!(entry.consecutive_failures, 0);
    }
}
