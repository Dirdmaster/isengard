//! Integration test: HttpChannel POSTs to mock URL with custom header.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::http::{HttpChannel, HttpConfig};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn http_send_posts_with_default_body_and_custom_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("x-isengard-test", "yes"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut headers = HashMap::new();
    headers.insert("x-isengard-test".into(), "yes".into());

    let cfg = HttpConfig {
        url: format!("{}/hook", server.uri()),
        headers,
        body_template: None,
        kinds: vec!["update.success".into()],
        tokens_per_minute: None,
    };
    let ch = HttpChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "ok".into(),
        ..Default::default()
    };
    ch.send(&ev).await.expect("http send should succeed");
}
