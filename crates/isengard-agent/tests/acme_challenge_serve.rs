//! Integration: verify Pingora's :8080 listener serves HTTP-01 challenge
//! tokens from `ProxyState.acme_challenges` and 404s unknown tokens. This is
//! the path Let's Encrypt hits during ACME validation; if it breaks, every
//! cert renewal fails silently until expiry.

use isengard_agent::proxy::ProxyState;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn challenge_token_returns_key_authorization() {
    let state = ProxyState::new();
    state
        .acme_challenges
        .install("test-token", "test-key-auth-value")
        .await;

    let proxy_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let body = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/.well-known/acme-challenge/test-token",
            proxy_port
        ))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(body, "test-key-auth-value");

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
}

#[tokio::test]
async fn challenge_unknown_token_returns_404() {
    let state = ProxyState::new();
    let proxy_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let st = state.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_task = tokio::spawn(async move {
        isengard_agent::proxy::server::run_for_test(st, proxy_port, Some(shutdown_rx)).await;
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/.well-known/acme-challenge/nope",
            proxy_port
        ))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
}
