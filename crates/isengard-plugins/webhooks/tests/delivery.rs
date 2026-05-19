//! Integration tests: delivery worker against a wiremock server.
//!
//! Covers: 200 -> success, 5xx -> retry, 4xx -> failed (no retry), and
//! signature header presence + correctness.

#![allow(clippy::result_large_err)]

use std::time::Duration;

use isengard_plugin_webhooks::sign::{SIGNATURE_HEADER, verify_signature};
use isengard_plugin_webhooks::worker::tick_once;
use isengard_storage::Inventory;
use isengard_storage::webhook::{DeliveryStatus, InsertDelivery, InsertWebhook, Webhook};
use reqwest::Client;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

async fn open_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
}

async fn make_webhook(inv: &Inventory, url: String, secret: &str) -> Webhook {
    inv.insert_webhook(InsertWebhook {
        url,
        secret: secret.into(),
        event_kinds: "*".into(),
        enabled: true,
    })
    .await
    .expect("insert webhook")
}

async fn enqueue(inv: &Inventory, webhook_id: i64, payload: &str) -> i64 {
    inv.insert_delivery(InsertDelivery {
        webhook_id,
        event_kind: "update.success".into(),
        payload_json: payload.into(),
    })
    .await
    .expect("insert delivery")
    .id
}

#[tokio::test]
async fn success_marks_row_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header_exists(SIGNATURE_HEADER))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let inv = open_inv().await;
    let w = make_webhook(&inv, format!("{}/hook", server.uri()), "k").await;
    let id = enqueue(&inv, w.id, r#"{"kind":"update.success"}"#).await;

    let client = build_client();
    tick_once(&inv, &client, 10).await.expect("tick");

    let row = inv.get_delivery(id).await.unwrap().unwrap();
    assert_eq!(row.status, DeliveryStatus::Success);
    assert_eq!(row.attempts, 1);
}

#[tokio::test]
async fn server_5xx_schedules_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let inv = open_inv().await;
    let w = make_webhook(&inv, format!("{}/hook", server.uri()), "k").await;
    let id = enqueue(&inv, w.id, "{}").await;

    let client = build_client();
    tick_once(&inv, &client, 10).await.expect("tick");

    let row = inv.get_delivery(id).await.unwrap().unwrap();
    assert_eq!(row.status, DeliveryStatus::Pending);
    assert_eq!(row.attempts, 1);
    assert!(row.next_retry_at.is_some(), "retry scheduled");
    assert!(row.last_error.as_deref().unwrap().contains("503"));
}

#[tokio::test]
async fn server_4xx_marks_row_failed_no_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;

    let inv = open_inv().await;
    let w = make_webhook(&inv, format!("{}/hook", server.uri()), "k").await;
    let id = enqueue(&inv, w.id, "{}").await;

    let client = build_client();
    tick_once(&inv, &client, 10).await.expect("tick");

    let row = inv.get_delivery(id).await.unwrap().unwrap();
    assert_eq!(row.status, DeliveryStatus::Failed);
    assert!(row.next_retry_at.is_none(), "4xx must not retry");
    assert_eq!(row.attempts, 1);
}

#[tokio::test]
async fn signature_header_is_correct_hmac() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(move |req: &Request| {
            let sig = req
                .headers
                .get(SIGNATURE_HEADER)
                .map(|v| v.to_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            // Verify against the body the server received with the known secret.
            assert!(
                verify_signature(b"my-secret", &req.body, &sig),
                "signature mismatch: header={sig}"
            );
            ResponseTemplate::new(200)
        })
        .mount(&server)
        .await;

    let inv = open_inv().await;
    let w = make_webhook(&inv, format!("{}/hook", server.uri()), "my-secret").await;
    let _id = enqueue(&inv, w.id, r#"{"kind":"webhook.test","summary":"hi"}"#).await;

    let client = build_client();
    tick_once(&inv, &client, 10).await.expect("tick");
}

#[tokio::test]
async fn five_consecutive_5xx_marks_exhausted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let inv = open_inv().await;
    let w = make_webhook(&inv, format!("{}/hook", server.uri()), "k").await;
    let id = enqueue(&inv, w.id, "{}").await;

    let client = build_client();

    // Five ticks: each one bumps attempts and reschedules. Between ticks we
    // backdate `next_retry_at` so the next claim picks the row up.
    for _ in 0..5 {
        tick_once(&inv, &client, 10).await.expect("tick");
        // Force the delivery back into "due now" so the next tick claims it.
        let row = inv.get_delivery(id).await.unwrap().unwrap();
        if matches!(
            row.status,
            DeliveryStatus::Exhausted | DeliveryStatus::Failed
        ) {
            break;
        }
        let now = chrono::Utc::now();
        inv.mark_delivery_pending(
            id,
            now,
            row.attempts,
            now - chrono::Duration::seconds(1),
            row.last_error.clone().unwrap_or_default().as_str(),
        )
        .await
        .expect("rebump");
    }

    let row = inv.get_delivery(id).await.unwrap().unwrap();
    assert_eq!(row.status, DeliveryStatus::Exhausted);
    assert_eq!(row.attempts, 5);
}
