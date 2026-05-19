//! Discord interactive messages + signature verify.
//!
//! Boundaries are mocked at the HTTP layer with `wiremock`. The
//! subscriber-style test uses the real `Inventory` (in-memory sqlite) so the
//! `set_discord_approval_message_metadata` round-trip is genuine.

#![allow(clippy::result_large_err)]

use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use isengard_plugin_notifier::channel::InlineButton;
use isengard_plugin_notifier::discord::{
    DiscordInteractive, edit_discord_message_text, verify_discord_signature,
};
use isengard_storage::host_action::{InsertPendingApproval, UpdateApprovalBody};
use isengard_storage::{EnrollHost, HostId, Inventory};
use rand::rngs::OsRng;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BOT_TOKEN: &str = "TEST_DISCORD_BOT_TOKEN_9C";
const CHANNEL_ID: i64 = 9_876_543_210;

fn mk_interactive(api_base: String) -> DiscordInteractive {
    // SAFETY: tests run in parallel by default; we set the env var here just
    // before constructing so the from_env() picks it up. The ENV race is
    // tolerable for this isolated test crate because no other test reads
    // ISENGARD_DISCORD_BOT_TOKEN at the time the channel is built.
    unsafe {
        std::env::set_var("ISENGARD_DISCORD_BOT_TOKEN", BOT_TOKEN);
    }
    DiscordInteractive::from_env(CHANNEL_ID, Some(api_base))
        .expect("from_env Result")
        .expect("from_env Some")
}

#[tokio::test]
async fn send_action_row_payload_shape() {
    let server = MockServer::start().await;

    let expected = json!({
        "content": "approval needed",
        "components": [
            {
                "type": 1,
                "components": [
                    { "type": 2, "style": 3, "label": "Approve",    "custom_id": "apv:01ABC:approve" },
                    { "type": 2, "style": 4, "label": "Reject",     "custom_id": "apv:01ABC:reject" },
                    { "type": 2, "style": 2, "label": "Snooze 24h", "custom_id": "apv:01ABC:snooze:24" }
                ]
            }
        ]
    });

    Mock::given(method("POST"))
        .and(path(format!("/channels/{CHANNEL_ID}/messages")))
        .and(header("authorization", format!("Bot {BOT_TOKEN}")))
        .and(body_partial_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "111222333",
            "channel_id": CHANNEL_ID.to_string(),
            "type": 0
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dc = mk_interactive(server.uri());
    let buttons = vec![
        InlineButton::new("Approve", "apv:01ABC:approve"),
        InlineButton::new("Reject", "apv:01ABC:reject"),
        InlineButton::new("Snooze 24h", "apv:01ABC:snooze:24"),
    ];
    let sent = dc
        .send_action_row("approval needed", &buttons)
        .await
        .expect("send_action_row");
    assert_eq!(sent.message_id, 111_222_333);
    assert_eq!(sent.channel_id, CHANNEL_ID);
}

#[tokio::test]
async fn send_action_row_4xx_returns_err_no_panic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/channels/{CHANNEL_ID}/messages")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "message": "401: Unauthorized",
            "code": 0
        })))
        .mount(&server)
        .await;

    let dc = mk_interactive(server.uri());
    let r = dc
        .send_action_row("hi", &[InlineButton::new("a", "apv:x:approve")])
        .await;
    assert!(r.is_err(), "expected Err on 401, got {r:?}");
    let msg = format!("{}", r.unwrap_err());
    assert!(
        msg.contains("non-success") || msg.contains("401"),
        "msg={msg}"
    );
}

#[tokio::test]
async fn edit_discord_message_text_shape_clears_components() {
    let server = MockServer::start().await;

    // Discord uses PATCH for message edits. Body should carry components: []
    // so the action row is removed in place.
    struct EmptyComponents;
    impl wiremock::Match for EmptyComponents {
        fn matches(&self, request: &Request) -> bool {
            let v: serde_json::Value = match serde_json::from_slice(&request.body) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("components")
                .and_then(|c| c.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false)
        }
    }

    let expected = json!({ "content": "decided: approved" });

    Mock::given(method("PATCH"))
        .and(path(format!("/channels/{CHANNEL_ID}/messages/{}", 42_i64)))
        .and(header("authorization", format!("Bot {BOT_TOKEN}")))
        .and(body_partial_json(expected))
        .and(EmptyComponents)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "42",
            "channel_id": CHANNEL_ID.to_string()
        })))
        .expect(1)
        .mount(&server)
        .await;

    edit_discord_message_text(
        Some(&server.uri()),
        BOT_TOKEN,
        CHANNEL_ID,
        42,
        "decided: approved",
    )
    .await
    .expect("edit");
}

#[tokio::test]
async fn verify_discord_signature_accepts_valid_rejects_tampered() {
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let public_hex = hex::encode(signing.verifying_key().to_bytes());

    let timestamp = b"1700000000";
    let body = br#"{"type":1}"#;
    let mut signed = Vec::new();
    signed.extend_from_slice(timestamp);
    signed.extend_from_slice(body);
    let sig_hex = hex::encode(signing.sign(&signed).to_bytes());

    // Valid: no error.
    verify_discord_signature(&public_hex, timestamp, body, &sig_hex)
        .expect("valid signature should verify");

    // Tampered body: error.
    let tampered = br#"{"type":2}"#;
    assert!(verify_discord_signature(&public_hex, timestamp, tampered, &sig_hex).is_err());

    // Tampered signature (last hex char flipped): error.
    let mut bad_sig = sig_hex.clone();
    let last = bad_sig.pop().unwrap();
    bad_sig.push(if last == 'a' { 'b' } else { 'a' });
    assert!(verify_discord_signature(&public_hex, timestamp, body, &bad_sig).is_err());

    // Wrong key: error.
    let other = SigningKey::generate(&mut rng);
    let other_pub = hex::encode(other.verifying_key().to_bytes());
    assert!(verify_discord_signature(&other_pub, timestamp, body, &sig_hex).is_err());
}

// -------- subscriber-style: real Inventory, mocked Discord ----------------

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
    })
    .await
    .expect("enroll")
}

#[tokio::test]
async fn approval_send_persists_discord_metadata_via_inventory() {
    let inv = fresh_inv().await;
    let host_id = enroll(&inv, "host-fp-9c").await;
    let body = UpdateApprovalBody {
        host_id,
        stack: "web".into(),
        service: "api".into(),
        container_name: "web_api_1".into(),
        image: "ghcr.io/example/web".into(),
        current_digest: "sha256:aaaaaaaa".into(),
        proposed_digest: "sha256:bbbbbbbb".into(),
        diff_url: None,
        approver_channel: Some("discord".into()),
    };
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body,
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: Some("discord".into()),
        })
        .await
        .expect("insert pending_approval");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/channels/{CHANNEL_ID}/messages")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "777888",
            "channel_id": CHANNEL_ID.to_string()
        })))
        .mount(&server)
        .await;
    let dc = mk_interactive(server.uri());

    let buttons = vec![
        InlineButton::new("Approve", format!("apv:{}:approve", row.action_id)),
        InlineButton::new("Reject", format!("apv:{}:reject", row.action_id)),
        InlineButton::new("Snooze 24h", format!("apv:{}:snooze:24", row.action_id)),
    ];
    let sent = dc
        .send_action_row("approval needed", &buttons)
        .await
        .expect("send_action_row");
    inv.set_discord_approval_message_metadata(&row.action_id, sent.channel_id, sent.message_id)
        .await
        .expect("persist metadata");

    let after = inv
        .get_pending_approval(&row.action_id)
        .await
        .expect("get")
        .expect("row exists");
    let meta = after.metadata_json.expect("metadata persisted");
    assert_eq!(meta["notifier_discord_channel_id"], json!(CHANNEL_ID));
    assert_eq!(meta["notifier_discord_message_id"], json!(777_888));
}
