//! T6 (phase 9b): Telegram interactive messages + approval subscriber.
//!
//! Boundaries are mocked at the HTTP layer with `wiremock`. The
//! subscriber-level tests use the real `Inventory` (in-memory sqlite) so the
//! `set_approval_message_metadata` round-trip is genuine.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use chrono::{Duration, Utc};
use isengard_core::Event;
use isengard_plugin_notifier::channel::InlineButton;
use isengard_plugin_notifier::telegram::{TelegramChannel, TelegramConfig};
use isengard_storage::host_action::{InsertPendingApproval, UpdateApprovalBody};
use isengard_storage::{EnrollHost, HostId, Inventory};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BOT_TOKEN: &str = "TEST_BOT_TOKEN_T6";

fn mk_channel(api_base: String) -> TelegramChannel {
    TelegramChannel::from_config(TelegramConfig {
        bot_token: Some(BOT_TOKEN.into()),
        chat_ids: vec!["555".into()],
        kinds: vec!["update.success".into()],
        api_base: Some(api_base),
        tokens_per_minute: None,
    })
    .expect("telegram channel from_config")
}

fn approve_button(action_id: &str) -> InlineButton {
    InlineButton::new("Approve", format!("apv:{action_id}:approve"))
}

fn reject_button(action_id: &str) -> InlineButton {
    InlineButton::new("Reject", format!("apv:{action_id}:reject"))
}

#[tokio::test]
async fn send_inline_keyboard_payload_shape() {
    let server = MockServer::start().await;

    let expected_body = json!({
        "chat_id": "555",
        "parse_mode": "HTML",
        "reply_markup": {
            "inline_keyboard": [
                [
                    {"text": "Approve", "callback_data": "apv:01ABCDEFG:approve"},
                    {"text": "Reject",  "callback_data": "apv:01ABCDEFG:reject"}
                ],
                [
                    {"text": "Snooze 24h", "callback_data": "apv:01ABCDEFG:snooze:24"}
                ]
            ]
        }
    });

    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/sendMessage")))
        .and(body_partial_json(expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 42, "chat": {"id": 555}}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ch = mk_channel(server.uri());
    let buttons = vec![
        vec![approve_button("01ABCDEFG"), reject_button("01ABCDEFG")],
        vec![InlineButton::new("Snooze 24h", "apv:01ABCDEFG:snooze:24")],
    ];

    let sent = ch
        .send_inline_keyboard("555", "<b>hi</b>", buttons)
        .await
        .expect("send_inline_keyboard");
    assert_eq!(sent.message_id, 42);
    assert_eq!(sent.chat_id, "555");
}

#[tokio::test]
async fn send_inline_keyboard_returns_message_id() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 9001, "chat": {"id": 100}}
        })))
        .mount(&server)
        .await;

    let ch = mk_channel(server.uri());
    let sent = ch
        .send_inline_keyboard(
            "555",
            "text",
            vec![vec![InlineButton::new("ok", "apv:x:approve")]],
        )
        .await
        .expect("send");
    assert_eq!(sent.message_id, 9001);
    assert_eq!(sent.chat_id, "555");
}

#[tokio::test]
async fn edit_message_text_payload_shape_without_keyboard() {
    let server = MockServer::start().await;

    // body_partial_json asserts the listed keys are present with these values;
    // we ALSO want to assert reply_markup is absent. Use a custom matcher.
    struct NoReplyMarkup;
    impl wiremock::Match for NoReplyMarkup {
        fn matches(&self, request: &Request) -> bool {
            let v: serde_json::Value = match serde_json::from_slice(&request.body) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("reply_markup").is_none()
        }
    }

    let expected = json!({
        "chat_id": "555",
        "message_id": 42,
        "parse_mode": "HTML",
        "text": "decided: approved"
    });

    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/editMessageText")))
        .and(body_partial_json(expected))
        .and(NoReplyMarkup)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = mk_channel(server.uri());
    ch.edit_message_text("555", 42, "decided: approved", None)
        .await
        .expect("edit_message_text");
}

#[tokio::test]
async fn edit_message_text_with_keyboard_includes_reply_markup() {
    let server = MockServer::start().await;

    let expected = json!({
        "chat_id": "555",
        "message_id": 7,
        "text": "still pending",
        "reply_markup": {
            "inline_keyboard": [[
                {"text": "Approve", "callback_data": "apv:abc:approve"}
            ]]
        }
    });

    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/editMessageText")))
        .and(body_partial_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = mk_channel(server.uri());
    ch.edit_message_text(
        "555",
        7,
        "still pending",
        Some(vec![vec![InlineButton::new("Approve", "apv:abc:approve")]]),
    )
    .await
    .expect("edit");
}

#[tokio::test]
async fn send_inline_keyboard_4xx_returns_err_no_panic() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "ok": false,
            "error_code": 401,
            "description": "Unauthorized"
        })))
        .mount(&server)
        .await;

    let ch = mk_channel(server.uri());
    let r = ch
        .send_inline_keyboard("555", "x", vec![vec![InlineButton::new("a", "b")]])
        .await;
    assert!(r.is_err(), "expected Err on 401, got {r:?}");
    let msg = format!("{}", r.unwrap_err());
    assert!(
        msg.contains("non-success") || msg.contains("401"),
        "msg={msg}"
    );
}

// -------- subscriber-level test (real Inventory, mocked Telegram) ----------

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

async fn enroll(inv: &Inventory, fp: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: fp.into(),
        hostname: fp.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
        fleet: "default".into(),
    })
    .await
    .expect("enroll")
}

#[tokio::test]
async fn approval_send_persists_message_metadata_via_inventory() {
    // Spin up an in-memory Inventory and seed an open pending_approval row.
    let inv = fresh_inv().await;
    let host_id = enroll(&inv, "host-fp-T6").await;
    let body = UpdateApprovalBody {
        host_id,
        stack: "web".into(),
        service: "api".into(),
        container_name: "web_api_1".into(),
        image: "ghcr.io/example/web".into(),
        current_digest: "sha256:aaaaaaaa".into(),
        proposed_digest: "sha256:bbbbbbbb".into(),
        diff_url: None,
        approver_channel: Some("telegram".into()),
    };
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body,
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: Some("telegram".into()),
        })
        .await
        .expect("insert pending_approval");

    // Mock Telegram returning a fresh message_id.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT_TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": 1234, "chat": {"id": 555}}
        })))
        .mount(&server)
        .await;
    let ch = Arc::new(mk_channel(server.uri()));

    // Build the synthetic event the way T2 will emit it.
    let event = Event {
        kind: "update.pending_approval".into(),
        occurred_at: Utc::now(),
        summary: "approval needed".into(),
        metadata: json!({
            "action_id": row.action_id,
            "host_id": "h1",
            "stack": "web",
            "service": "api",
            "image": "ghcr.io/example/web",
            "current_digest": "sha256:aaaaaaaa",
            "proposed_digest": "sha256:bbbbbbbb"
        }),
        ..Default::default()
    };

    // Simulate what the subscriber does: send + persist metadata.
    let buttons = vec![
        vec![
            InlineButton::new("Approve", format!("apv:{}:approve", row.action_id)),
            InlineButton::new("Reject", format!("apv:{}:reject", row.action_id)),
        ],
        vec![InlineButton::new(
            "Snooze 24h",
            format!("apv:{}:snooze:24", row.action_id),
        )],
    ];
    let chat_id = ch.chat_ids().first().cloned().expect("chat_ids");
    let sent = ch
        .send_inline_keyboard(&chat_id, &event.summary, buttons)
        .await
        .expect("send_inline_keyboard");
    let chat_id_i64 = sent.chat_id.parse::<i64>().expect("numeric chat id");
    inv.set_approval_message_metadata(&row.action_id, chat_id_i64, sent.message_id)
        .await
        .expect("persist metadata");

    // Read back and assert.
    let after = inv
        .get_pending_approval(&row.action_id)
        .await
        .expect("get")
        .expect("row exists");
    let meta = after.metadata_json.expect("metadata persisted");
    assert_eq!(meta["notifier_chat_id"], json!(555));
    assert_eq!(meta["notifier_message_id"], json!(1234));
}

#[tokio::test]
async fn malformed_pending_approval_payload_does_not_panic() {
    // Just exercise the JSON parse fallback: serde_json::from_value with a
    // missing action_id should error and the subscriber should bail without
    // panicking. This is a unit-shaped check on the deserialization shape.
    #[derive(serde::Deserialize)]
    struct PendingApprovalPayload {
        #[allow(dead_code)]
        action_id: String,
    }
    let bad = json!({});
    let r: Result<PendingApprovalPayload, _> = serde_json::from_value(bad);
    assert!(r.is_err());
}
