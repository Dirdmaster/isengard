//! Integration test: TelegramChannel POSTs to a mock /sendMessage endpoint.

#![allow(clippy::result_large_err)]

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::telegram::{TelegramChannel, TelegramConfig};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn telegram_send_posts_to_send_message_endpoint() {
    let server = MockServer::start().await;
    let bot_token = "TEST_TOKEN_123";

    Mock::given(method("POST"))
        .and(path(format!("/bot{bot_token}/sendMessage")))
        .and(body_json(serde_json::json!({
            "chat_id": "555",
            "text": "[isengard] update.success\ncontainer: web\nimage: nginx:1.25\ndigest: sha256:aaa → sha256:bbb\nupdated web"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;

    let cfg = TelegramConfig {
        bot_token: Some(bot_token.into()),
        chat_ids: vec!["555".into()],
        kinds: vec!["update.success".into()],
        api_base: Some(server.uri()),
        tokens_per_minute: None,
    };
    let ch = TelegramChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "updated web".into(),
        container_name: Some("web".into()),
        image: Some("nginx:1.25".into()),
        old_digest: Some("sha256:aaa".into()),
        new_digest: Some("sha256:bbb".into()),
        ..Default::default()
    };
    ch.send(&ev).await.expect("telegram send should succeed");
    // Mock auto-verifies on drop that the expected request was made.
}
