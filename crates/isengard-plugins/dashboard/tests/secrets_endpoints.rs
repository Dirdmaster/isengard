//! Integration tests for the v0.3.6 secrets-store REST endpoints
//! (`POST/PUT/GET/DELETE /api/v1/secrets[/<name>]`).
//!
//! Mirrors the shape of `enrollment_endpoints.rs`: build an in-memory
//! controller-handle bundle, mount the secrets router, drive requests
//! through `tower::ServiceExt::oneshot`. The on-disk encryption path is
//! exercised end-to-end (a master key IS provided), so a successful
//! `put` -> `list` round-trip confirms the controller's
//! ChaCha20-Poly1305 flow works without leaking ciphertext to JSON.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_controller::secrets::SecretsStore;
use isengard_plugin_dashboard::secrets;
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn setup_app(unlocked: bool) -> (axum::Router, Arc<ControllerHandles>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment_svc = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    let secrets_store = if unlocked {
        // Deterministic 32-byte test key. Real installs use a fresh
        // openssl-rand-32 value; for round-trip tests any constant works.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        Arc::new(SecretsStore::new(inv.clone(), key))
    } else {
        Arc::new(SecretsStore::new_locked(inv.clone()))
    };

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
        secrets: secrets_store,
        ca,
    });

    let app = secrets::router(handles.clone());
    (app, handles)
}

async fn body_to_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn post_then_get_then_delete_round_trip() {
    let (app, _h) = setup_app(true).await;

    // POST /api/v1/secrets
    let create_req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "cf_token", "value": "abc-xyz-123"}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET /api/v1/secrets
    let list_req = Request::builder()
        .method("GET")
        .uri("/secrets")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(list_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "cf_token");
    // CRITICAL: the list endpoint MUST NEVER expose the value.
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        !raw.contains("abc-xyz-123"),
        "list response must not contain plaintext: {raw}"
    );
    assert!(!raw.contains("ciphertext"));

    // DELETE /api/v1/secrets/cf_token
    let del_req = Request::builder()
        .method("DELETE")
        .uri("/secrets/cf_token")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(del_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET again -> empty
    let list_req = Request::builder()
        .method("GET")
        .uri("/secrets")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(list_req).await.unwrap();
    let body = body_to_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn post_existing_name_returns_409() {
    let (app, _h) = setup_app(true).await;

    let body1 = json!({"name": "dup", "value": "v1"}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(body1))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body2 = json!({"name": "dup", "value": "v2"}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(body2))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_replaces_existing() {
    let (app, _h) = setup_app(true).await;

    let req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "k", "value": "first"}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = Request::builder()
        .method("PUT")
        .uri("/secrets/k")
        .header("content-type", "application/json")
        .body(Body::from(json!({"value": "second"}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn delete_missing_returns_404() {
    let (app, _h) = setup_app(true).await;
    let req = Request::builder()
        .method("DELETE")
        .uri("/secrets/never-existed")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_without_master_key_returns_503() {
    let (app, _h) = setup_app(false).await;
    let req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(json!({"name": "k", "value": "v"}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn post_invalid_name_returns_400() {
    let (app, _h) = setup_app(true).await;
    let req = Request::builder()
        .method("POST")
        .uri("/secrets")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "has spaces!!", "value": "v"}).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_omits_values_for_many_secrets() {
    let (app, _h) = setup_app(true).await;

    for (n, v) in [
        ("alpha", "secret-A"),
        ("beta", "secret-B"),
        ("gamma", "secret-C"),
    ] {
        let req = Request::builder()
            .method("POST")
            .uri("/secrets")
            .header("content-type", "application/json")
            .body(Body::from(json!({"name": n, "value": v}).to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let req = Request::builder()
        .method("GET")
        .uri("/secrets")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = body_to_json(resp).await;
    let raw = serde_json::to_string(&body).unwrap();
    for v in ["secret-A", "secret-B", "secret-C"] {
        assert!(!raw.contains(v), "list endpoint leaked value {v}: {raw}");
    }
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 3);
}
