//! Task 20: healthcheck eviction emits a `routing.upstream.health_changed`
//! event onto the agent's outbound event channel.
//!
//! Setup: a tiny TCP responder that always returns 500. Wire it into a
//! `ProxyState` with an installed event sink, run the healthcheck loop, and
//! assert an event lands on the channel after the eviction transition.

use std::time::Duration;
use tokio::sync::mpsc;

use isengard_agent::proxy::{ProxyState, Upstream};

async fn fail_responder() -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                tokio::spawn(async move {
                    let mut b = [0u8; 1024];
                    let _ = s.read(&mut b).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 500 X\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
async fn evicting_upstream_emits_health_changed_event() {
    let addr = fail_responder().await;
    let state = ProxyState::new();
    let (event_tx, mut event_rx) = mpsc::channel::<isengard_core::Event>(16);
    state.set_event_sink(event_tx);
    // Give set_event_sink's spawned task a moment to install the sender
    // before the healthcheck loop starts emitting.
    tokio::time::sleep(Duration::from_millis(20)).await;

    {
        let mut up = state.upstreams.write().await;
        up.set(
            "e.test",
            Upstream {
                container_id: "c".into(),
                addr,
                healthy: true,
                health_path: Some("/h".into()),
                health_interval: Duration::from_millis(50),
                consecutive_failures: 0,
            },
        );
    }
    isengard_agent::proxy::healthcheck::spawn_loops(state.clone());

    let ev = tokio::time::timeout(Duration::from_millis(800), event_rx.recv())
        .await
        .expect("event arrived in time")
        .expect("channel still open");
    assert_eq!(ev.kind, "routing.upstream.health_changed");
    assert!(
        ev.summary.contains("e.test"),
        "summary missing host: {}",
        ev.summary
    );
    assert!(
        ev.summary.contains("unhealthy"),
        "summary missing state: {}",
        ev.summary
    );
}
