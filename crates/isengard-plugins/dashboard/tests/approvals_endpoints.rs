//! Integration tests for `/api/v1/approvals` + Telegram webhook callback
//! (Phase 9 Plan B, T4).
//!
//! Builds the approvals router against an in-memory `Inventory` and verifies:
//! - GET list / single / round-trip after a storage-side insert
//! - POST decide (approve/reject/snooze), including the side-effects:
//!   `force_update` HostAction queued on approve; service-scope policy
//!   `paused_until` upserted on snooze
//! - Validation: 409 (already decided), 422 (unknown decision), 400 (snooze
//!   without hours)
//! - Telegram callback: 401 on bad/missing secret, 400 on malformed
//!   callback_data, decision applied on valid request
//!
//! The Telegram editMessageText round-trip is intentionally NOT exercised
//! here (no bot token in env -> the helper is skipped). The notifier-side
//! tests (`telegram_interactive.rs`) cover the wire format.

use std::sync::{Arc, OnceLock};

use tokio::sync::{Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_core::policy::PolicyScopeType;
use isengard_plugin_dashboard::approvals::{self, ApprovalDto, DecisionResponseDto};
use isengard_storage::host_action::{ApprovalState, InsertPendingApproval, UpdateApprovalBody};
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use isengard_storage::{EnrollHost, HostId};
use rand::rngs::OsRng;
use tower::ServiceExt;

const WEBHOOK_SECRET: &str = "test-secret-123";

async fn setup_app() -> (axum::Router, Arc<ControllerHandles>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    let handles = Arc::new(ControllerHandles {
        inventory: inv,
        journal,
        bus,
        routing,
        enrollment,
        revocation,
        db_path: std::path::PathBuf::from(":memory:"),
        log_fanout: isengard_controller::log_fanout::LogFanout::new(),
    });
    let app = approvals::router(handles.clone());
    (app, handles)
}

async fn enroll(inv: &Inventory) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: "fp-test".into(),
        hostname: "h-test".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
        fleet: "default".into(),
    })
    .await
    .unwrap()
}

fn body_for(host_id: HostId, stack: &str, service: &str) -> UpdateApprovalBody {
    UpdateApprovalBody {
        host_id,
        stack: stack.into(),
        service: service.into(),
        container_name: format!("{stack}_{service}_1"),
        image: "ghcr.io/example/web".into(),
        current_digest: "sha256:aaaaaaaa".into(),
        proposed_digest: "sha256:bbbbbbbb".into(),
        diff_url: Some("https://example.com/diff".into()),
        approver_channel: Some("ops-team".into()),
    }
}

async fn seed_open_approval(inv: &Inventory, stack: &str, service: &str) -> (HostId, String) {
    let host_id = enroll(inv).await;
    let body = body_for(host_id, stack, service);
    let row = inv
        .insert_pending_approval(InsertPendingApproval {
            body,
            expires_at: Utc::now() + Duration::hours(24),
            approver_channel: Some("ops-team".into()),
        })
        .await
        .unwrap();
    (host_id, row.action_id)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_telegram(uri: &str, body: serde_json::Value, secret_header: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(s) = secret_header {
        b = b.header("x-telegram-bot-api-secret-token", s);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Process-wide guard for tests that touch `ISENGARD_TELEGRAM_WEBHOOK_SECRET`.
/// Cargo runs tests in parallel; the env var is process-global, so two tests
/// fighting over it will see each other's writes. Acquiring this async mutex
/// at the top of every env-touching test serializes them and the guard can
/// safely be held across `.await` points.
async fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().await
}

fn set_webhook_secret() {
    // SAFETY: Tests touch a process-global env. Callers hold `env_lock()`
    // for the duration of the test so other env-aware tests don't observe
    // the partial state.
    unsafe {
        std::env::set_var("ISENGARD_TELEGRAM_WEBHOOK_SECRET", WEBHOOK_SECRET);
    }
}

fn clear_webhook_secret() {
    unsafe {
        std::env::remove_var("ISENGARD_TELEGRAM_WEBHOOK_SECRET");
    }
}

// ---------------------------------------------------------------------------
// GET /approvals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_approvals_empty_returns_empty_array() {
    let (app, _h) = setup_app().await;
    let resp = app.oneshot(get_req("/approvals")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test]
async fn post_then_get_returns_inserted_row() {
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    // GET /approvals/:id
    let resp = app
        .clone()
        .oneshot(get_req(&format!("/approvals/{action_id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: ApprovalDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.action_id, action_id);
    assert_eq!(dto.state, ApprovalState::PendingOpen);
    assert_eq!(dto.stack, "blog");
    assert_eq!(dto.service, "blog/web");

    // GET /approvals
    let list_resp = app.oneshot(get_req("/approvals")).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let listed: Vec<ApprovalDto> = serde_json::from_value(body_json(list_resp).await).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].action_id, action_id);
}

#[tokio::test]
async fn get_unknown_id_returns_404() {
    let (app, _h) = setup_app().await;
    let resp = app.oneshot(get_req("/approvals/01NOTREAL")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// POST /approvals/:id (decide)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decide_approve_marks_state_and_queues_apply_update() {
    let (app, h) = setup_app().await;
    let (host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "approve" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: DecisionResponseDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.approval.state, ApprovalState::PendingApproved);
    assert!(dto.dispatched_apply_update);
    assert!(dto.paused_until_set.is_none());

    // Verify a force_update HostAction was queued and is undelivered.
    let pending = h.inventory.pending_actions(host_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind.kind_str(), "force_update");
}

#[tokio::test]
async fn decide_reject_marks_state() {
    let (app, h) = setup_app().await;
    let (host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "reject" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: DecisionResponseDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.approval.state, ApprovalState::PendingRejected);
    assert!(!dto.dispatched_apply_update);

    // No force_update should have been queued.
    let pending = h.inventory.pending_actions(host_id).await.unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn decide_snooze_24h_writes_paused_until_on_service_policy() {
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let before = Utc::now();
    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "snooze", "snoozeHours": 24 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: DecisionResponseDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.approval.state, ApprovalState::PendingSnoozed);
    let pu = dto.paused_until_set.expect("paused_until_set populated");
    let expected_min = before + Duration::hours(23);
    let expected_max = before + Duration::hours(25);
    assert!(pu > expected_min && pu < expected_max, "paused_until={pu}");

    // Service-scope policy row should now exist with paused_until ~ +24h.
    let row = h
        .inventory
        .get_policy(PolicyScopeType::Service, "blog/web")
        .await
        .unwrap()
        .expect("service policy upserted");
    let policy_pu = row.body.paused_until.expect("paused_until set");
    assert!(policy_pu > expected_min && policy_pu < expected_max);
}

#[tokio::test]
async fn decide_on_already_decided_returns_409() {
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    // First call wins.
    let _ok = app
        .clone()
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "approve" }),
        ))
        .await
        .unwrap();

    // Second call should 409.
    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "approve" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn decide_with_invalid_decision_returns_422() {
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "explode" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn decide_snooze_without_hours_returns_400() {
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let resp = app
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "snooze" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// POST /notifier/callback/telegram
// ---------------------------------------------------------------------------

fn telegram_update(
    action_id: &str,
    decision: &str,
    snooze_hours: Option<u32>,
) -> serde_json::Value {
    let data = match (decision, snooze_hours) {
        ("snooze", Some(h)) => format!("apv:{action_id}:snooze:{h}"),
        (d, _) => format!("apv:{action_id}:{d}"),
    };
    serde_json::json!({
        "update_id": 1,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": 7, "username": "ops_user" },
            "data": data,
            "message": {
                "message_id": 99,
                "chat": { "id": 555, "type": "private" }
            }
        }
    })
}

#[tokio::test]
async fn telegram_callback_with_valid_secret_approves() {
    let _lock = env_lock().await;
    set_webhook_secret();
    let (app, h) = setup_app().await;
    let (host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = telegram_update(&action_id, "approve", None);
    let resp = app
        .oneshot(post_telegram(
            "/notifier/callback/telegram",
            body,
            Some(WEBHOOK_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Reply should be the answerCallbackQuery shape.
    let v = body_json(resp).await;
    assert_eq!(v["method"], "answerCallbackQuery");
    assert_eq!(v["callback_query_id"], "cb-1");
    assert_eq!(v["text"], "Approved");

    // State + side-effect.
    let row = h
        .inventory
        .get_pending_approval(&action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ApprovalState::PendingApproved);
    let pending = h.inventory.pending_actions(host_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind.kind_str(), "force_update");

    // Verify decided_by carries the username.
    assert_eq!(row.decided_by.as_deref(), Some("telegram:@ops_user"));
    clear_webhook_secret();
}

#[tokio::test]
async fn telegram_callback_with_bad_secret_returns_401() {
    let _lock = env_lock().await;
    set_webhook_secret();
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = telegram_update(&action_id, "approve", None);
    let resp = app
        .oneshot(post_telegram(
            "/notifier/callback/telegram",
            body,
            Some("wrong-secret"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Approval should still be pending_open.
    let row = h
        .inventory
        .get_pending_approval(&action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ApprovalState::PendingOpen);
    clear_webhook_secret();
}

#[tokio::test]
async fn telegram_callback_with_missing_header_returns_401() {
    let _lock = env_lock().await;
    set_webhook_secret();
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = telegram_update(&action_id, "approve", None);
    let resp = app
        .oneshot(post_telegram("/notifier/callback/telegram", body, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    clear_webhook_secret();
}

#[tokio::test]
async fn telegram_callback_with_unset_env_returns_401() {
    let _lock = env_lock().await;
    clear_webhook_secret();
    let (app, _h) = setup_app().await;
    let body = serde_json::json!({
        "update_id": 1,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": 7 },
            "data": "apv:01ABC:approve"
        }
    });
    let resp = app
        .oneshot(post_telegram(
            "/notifier/callback/telegram",
            body,
            Some(WEBHOOK_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn telegram_callback_with_malformed_data_returns_400() {
    let _lock = env_lock().await;
    set_webhook_secret();
    let (app, _h) = setup_app().await;

    let body = serde_json::json!({
        "update_id": 1,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": 7 },
            "data": "not-an-apv-payload"
        }
    });
    let resp = app
        .oneshot(post_telegram(
            "/notifier/callback/telegram",
            body,
            Some(WEBHOOK_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    clear_webhook_secret();
}

#[tokio::test]
async fn telegram_callback_snooze_path_writes_paused_until() {
    let _lock = env_lock().await;
    set_webhook_secret();
    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = telegram_update(&action_id, "snooze", Some(24));
    let resp = app
        .oneshot(post_telegram(
            "/notifier/callback/telegram",
            body,
            Some(WEBHOOK_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let row = h
        .inventory
        .get_pending_approval(&action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ApprovalState::PendingSnoozed);

    let pol = h
        .inventory
        .get_policy(PolicyScopeType::Service, "blog/web")
        .await
        .unwrap()
        .expect("service policy upserted");
    assert!(pol.body.paused_until.is_some());
    clear_webhook_secret();
}

// ---------------------------------------------------------------------------
// POST /notifier/callback/discord
// ---------------------------------------------------------------------------

const DISCORD_PUBLIC_KEY_ENV: &str = "ISENGARD_DISCORD_PUBLIC_KEY";

fn set_discord_public_key(hex_key: &str) {
    unsafe {
        std::env::set_var(DISCORD_PUBLIC_KEY_ENV, hex_key);
    }
}

fn clear_discord_public_key() {
    unsafe {
        std::env::remove_var(DISCORD_PUBLIC_KEY_ENV);
    }
}

/// Build a signed Discord callback request. Returns the request ready to feed
/// to `app.oneshot(...)`. `signing_key` is the test's ed25519 key; the public
/// key must already be set in `ISENGARD_DISCORD_PUBLIC_KEY` for the handler to
/// verify it.
fn discord_signed_request(
    signing_key: &SigningKey,
    body: serde_json::Value,
    timestamp: &str,
    flip_signature: bool,
) -> Request<Body> {
    let raw = body.to_string();
    let mut payload = Vec::new();
    payload.extend_from_slice(timestamp.as_bytes());
    payload.extend_from_slice(raw.as_bytes());
    let sig = signing_key.sign(&payload);
    let mut hex_sig = hex::encode(sig.to_bytes());
    if flip_signature {
        // Flip the last hex char so verification fails. Keeps the length valid
        // so we exercise the verify path itself, not a length pre-check.
        let last = hex_sig.pop().unwrap();
        hex_sig.push(if last == 'a' { 'b' } else { 'a' });
    }
    Request::builder()
        .method("POST")
        .uri("/notifier/callback/discord")
        .header("content-type", "application/json")
        .header("x-signature-ed25519", hex_sig)
        .header("x-signature-timestamp", timestamp)
        .body(Body::from(raw))
        .unwrap()
}

fn discord_message_component_body(
    action_id: &str,
    decision: &str,
    snooze_hours: Option<u32>,
) -> serde_json::Value {
    let custom_id = match (decision, snooze_hours) {
        ("snooze", Some(h)) => format!("apv:{action_id}:snooze:{h}"),
        (d, _) => format!("apv:{action_id}:{d}"),
    };
    serde_json::json!({
        "type": 3,
        "data": { "custom_id": custom_id, "component_type": 2 },
        "member": {
            "user": { "username": "ops_user", "id": "777" }
        },
        "message": {
            "id": "424242",
            "channel_id": "999999"
        }
    })
}

#[tokio::test]
async fn discord_callback_ping_returns_pong() {
    let _lock = env_lock().await;
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pub_hex = hex::encode(signing.verifying_key().to_bytes());
    set_discord_public_key(&pub_hex);

    let (app, _h) = setup_app().await;
    let body = serde_json::json!({ "type": 1 });
    let req = discord_signed_request(&signing, body, "1700000000", false);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v, serde_json::json!({ "type": 1 }));
    clear_discord_public_key();
}

#[tokio::test]
async fn discord_callback_message_component_approves() {
    let _lock = env_lock().await;
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pub_hex = hex::encode(signing.verifying_key().to_bytes());
    set_discord_public_key(&pub_hex);

    let (app, h) = setup_app().await;
    let (host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = discord_message_component_body(&action_id, "approve", None);
    let req = discord_signed_request(&signing, body, "1700000000", false);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["type"], 7);
    assert!(v["data"]["content"].as_str().unwrap().contains("Approved"));
    let components = v["data"]["components"].as_array().unwrap();
    assert!(components.is_empty(), "components should be cleared");

    let row = h
        .inventory
        .get_pending_approval(&action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ApprovalState::PendingApproved);
    assert_eq!(row.decided_by.as_deref(), Some("discord:@ops_user"));

    let pending = h.inventory.pending_actions(host_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind.kind_str(), "force_update");
    clear_discord_public_key();
}

#[tokio::test]
async fn discord_callback_bad_signature_returns_401() {
    let _lock = env_lock().await;
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pub_hex = hex::encode(signing.verifying_key().to_bytes());
    set_discord_public_key(&pub_hex);

    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    let body = discord_message_component_body(&action_id, "approve", None);
    let req = discord_signed_request(&signing, body, "1700000000", true);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // State unchanged.
    let row = h
        .inventory
        .get_pending_approval(&action_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ApprovalState::PendingOpen);
    clear_discord_public_key();
}

#[tokio::test]
async fn discord_callback_unset_public_key_returns_401() {
    let _lock = env_lock().await;
    clear_discord_public_key();
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);

    let (app, _h) = setup_app().await;
    let body = serde_json::json!({ "type": 1 });
    let req = discord_signed_request(&signing, body, "1700000000", false);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn discord_callback_malformed_custom_id_returns_400() {
    let _lock = env_lock().await;
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pub_hex = hex::encode(signing.verifying_key().to_bytes());
    set_discord_public_key(&pub_hex);

    let (app, _h) = setup_app().await;
    let body = serde_json::json!({
        "type": 3,
        "data": { "custom_id": "not-an-apv-payload", "component_type": 2 },
        "member": { "user": { "username": "ops_user", "id": "777" } }
    });
    let req = discord_signed_request(&signing, body, "1700000000", false);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    clear_discord_public_key();
}

#[tokio::test]
async fn discord_callback_already_decided_returns_update_message() {
    let _lock = env_lock().await;
    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let pub_hex = hex::encode(signing.verifying_key().to_bytes());
    set_discord_public_key(&pub_hex);

    let (app, h) = setup_app().await;
    let (_host_id, action_id) = seed_open_approval(&h.inventory, "blog", "blog/web").await;

    // Decide once via the dashboard path so the row is no longer pending_open.
    let _r = app
        .clone()
        .oneshot(post_json(
            &format!("/approvals/{action_id}"),
            serde_json::json!({ "decision": "approve" }),
        ))
        .await
        .unwrap();

    // Second decision via Discord callback should not error out; it should
    // return UPDATE_MESSAGE with an "Already decided" body so the user gets
    // feedback in the chat surface.
    let body = discord_message_component_body(&action_id, "reject", None);
    let req = discord_signed_request(&signing, body, "1700000001", false);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["type"], 7);
    let content = v["data"]["content"].as_str().unwrap();
    assert!(
        content.contains("Already decided"),
        "expected Already decided in {content}"
    );
    clear_discord_public_key();
}
