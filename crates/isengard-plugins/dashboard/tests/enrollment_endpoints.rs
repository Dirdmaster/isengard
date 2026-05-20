//! Integration tests for the enrollment-token + cert-revoke REST endpoints

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::enrollment::{self, MintTokenResponse, TokenListEntry};
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use tower::ServiceExt;

async fn setup_app() -> (axum::Router, Arc<ControllerHandles>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment_svc = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    let handles = Arc::new(ControllerHandles {
        inventory: inv.clone(),
        journal,
        bus,
        routing,
        enrollment: enrollment_svc,
        revocation,
        db_path: std::path::PathBuf::from(":memory:"),
        log_fanout: isengard_controller::log_fanout::LogFanout::new(),
        compose_broker: Arc::new(isengard_controller::compose_broker::ComposeBroker::new()),
        secrets: Arc::new(isengard_controller::secrets::SecretsStore::new_locked(
            inv.clone(),
        )),
        ca,
        ssh_ca: Arc::new(isengard_controller::ssh_ca::SshAuthority::for_tests().unwrap()),
    });

    let app = enrollment::router(handles.clone());
    (app, handles)
}

#[tokio::test]
async fn post_enrollment_token_returns_token() {
    let (app, _handles) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/enrollment/tokens")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"role":"agent","ttl_seconds":3600}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed: MintTokenResponse = serde_json::from_slice(&body).unwrap();
    assert!(!parsed.token.is_empty());
    assert!(parsed.expires_at.contains('T')); // RFC3339
}

#[tokio::test]
async fn post_enrollment_token_rejects_bad_role() {
    let (app, _handles) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/enrollment/tokens")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"role":"admin","ttl_seconds":3600}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_enrollment_token_rejects_out_of_range_ttl() {
    let (app, _handles) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/enrollment/tokens")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"role":"agent","ttl_seconds":0}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_enrollment_tokens_lists_active() {
    let (app, _handles) = setup_app().await;

    // Mint one token, then list.
    let mint = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/enrollment/tokens")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"role":"agent","ttl_seconds":3600}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mint.status(), StatusCode::CREATED);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/enrollment/tokens")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let entries: Vec<TokenListEntry> = serde_json::from_slice(&body).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].role, "agent");
    assert_eq!(entries[0].hash_prefix.len(), 16); // 8 bytes hex
    assert!(
        entries[0]
            .hash_prefix
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    );
}

#[tokio::test]
async fn delete_enrollment_token_marks_consumed() {
    let (app, handles) = setup_app().await;

    // Mint, then list to grab the prefix.
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/enrollment/tokens")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"role":"agent","ttl_seconds":3600}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let list_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/enrollment/tokens")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(list_resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let entries: Vec<TokenListEntry> = serde_json::from_slice(&body).unwrap();
    let prefix = entries[0].hash_prefix.clone();

    let del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/enrollment/tokens/{prefix}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);

    // After delete the active list should be empty.
    let after = handles.inventory.list_active_tokens().await.unwrap();
    assert!(after.is_empty(), "expected no active tokens after revoke");
}

#[tokio::test]
async fn delete_enrollment_token_unknown_returns_404() {
    let (app, _handles) = setup_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/enrollment/tokens/0011223344556677")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_host_cert_returns_404_for_unknown_host() {
    let (app, _handles) = setup_app().await;
    let bogus = ulid::Ulid::new().to_string();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/hosts/{bogus}/cert"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_host_cert_revokes_active_cert() {
    let (app, handles) = setup_app().await;

    // Enroll a host via the EnrollmentService so we have a real cert row to
    // revoke (rather than just a host row with no cert). Redeem
    // requires the packed `TK<bytes>.<fingerprint>` shape; pack the bare
    // base32 token mint returns before redeeming.
    let bare = handles
        .enrollment
        .mint(
            isengard_storage::enrollment_token::TokenRole::Agent,
            chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
    let bytes_vec = data_encoding::BASE32_NOPAD
        .decode(bare.as_bytes())
        .expect("mint returns base32");
    let bytes: [u8; 32] = bytes_vec.as_slice().try_into().expect("32 bytes");
    let fake_ca_pem = b"-----BEGIN CERTIFICATE-----\nFIXTURE\n-----END CERTIFICATE-----\n";
    let token = isengard_core::join_token::pack(&bytes, fake_ca_pem);
    let bundle = handles
        .enrollment
        .redeem(
            &token,
            isengard_controller::enrollment::HostInfo {
                hostname: "h-revoke".into(),
                os: "linux".into(),
                version: "0.1.0".into(),
            },
        )
        .await
        .unwrap();
    let host_id = bundle.host_id;

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/hosts/{host_id}/cert"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Active cert lookup should now return None.
    let active = handles
        .inventory
        .active_cert_for_host(host_id)
        .await
        .unwrap();
    assert!(active.is_none(), "expected cert to be revoked");
}
