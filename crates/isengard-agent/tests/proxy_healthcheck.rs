use isengard_agent::proxy::healthcheck::HealthChecker;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

async fn spawn_responder(should_200: bool) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let resp = if should_200 {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                    } else {
                        "HTTP/1.1 500 Internal\r\nContent-Length: 0\r\n\r\n"
                    };
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
async fn http_check_returns_healthy_for_2xx() {
    let addr = spawn_responder(true).await;
    let hc = HealthChecker::new("/healthz".into(), Duration::from_millis(500));
    let healthy = hc.check_once(addr).await;
    assert!(healthy);
}

#[tokio::test]
async fn http_check_returns_unhealthy_for_5xx() {
    let addr = spawn_responder(false).await;
    let hc = HealthChecker::new("/healthz".into(), Duration::from_millis(500));
    let healthy = hc.check_once(addr).await;
    assert!(!healthy);
}

#[tokio::test]
async fn tcp_only_check_returns_healthy_when_connect_succeeds() {
    let addr = spawn_responder(true).await;
    let hc = HealthChecker::tcp_only(Duration::from_millis(500));
    let healthy = hc.check_once(addr).await;
    assert!(healthy);
}
