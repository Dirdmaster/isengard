//! Integration test: DiscordChannel POSTs to mock webhook URL.

#![allow(clippy::result_large_err)]

use chrono::Utc;
use isengard_core::Event;
use isengard_plugin_notifier::channel::NotifyChannel;
use isengard_plugin_notifier::discord::{DiscordChannel, DiscordConfig};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn discord_send_posts_to_webhook_with_content_field() {
    let server = MockServer::start().await;
    let webhook_path = "/api/webhooks/123/abc";

    Mock::given(method("POST"))
        .and(path(webhook_path))
        .and(body_json(serde_json::json!({
            "content": "[isengard] update.success\ncontainer: web\nimage: nginx:1.25\nupdated"
        })))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let cfg = DiscordConfig {
        webhook_url: format!("{}{}", server.uri(), webhook_path),
        kinds: vec!["update.success".into()],
        tokens_per_minute: None,
    };
    let ch = DiscordChannel::from_config(cfg).unwrap();

    let ev = Event {
        kind: "update.success".into(),
        occurred_at: Utc::now(),
        summary: "updated".into(),
        container_name: Some("web".into()),
        image: Some("nginx:1.25".into()),
        ..Default::default()
    };
    ch.send(&ev).await.expect("discord send should succeed");
}
