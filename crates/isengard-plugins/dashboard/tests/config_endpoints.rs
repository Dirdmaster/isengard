//! Integration tests for the `isd configure` dashboard surface:
//!  - `GET /api/v1/config`
//!  - `GET /api/v1/config/schema`
//!  - `GET /api/v1/config/{key}`
//!  - `PUT /api/v1/config/{key}`
//!  - `DELETE /api/v1/config/{key}`
//!
//! Builds a real `ControllerHandles` over an in-memory inventory + an
//! unlocked secrets store so secret-typed keys round-trip end-to-end.
//! The router is exercised through `tower::ServiceExt::oneshot`, same
//! shape as `secrets_endpoints.rs` and `ssh_endpoints.rs`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_controller::secrets::SecretsStore;
use isengard_plugin_dashboard::config as dashboard_config;
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Builds a router with the configure routes mounted and the secrets
/// store unlocked so secret-typed keys can round-trip.
async fn setup_app() -> (Router, Arc<ControllerHandles>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment_svc = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    // Deterministic 32-byte test key for the secrets store.
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let secrets_store = Arc::new(SecretsStore::new(inv.clone(), key));
    let config_dispatcher =
        ControllerHandles::test_config_dispatcher(inv.clone(), secrets_store.clone());

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
        ssh_ca: Arc::new(isengard_controller::ssh_ca::SshAuthority::for_tests().unwrap()),
        config_dispatcher,
    });

    let app = dashboard_config::router(handles.clone());
    (app, handles)
}

/// Drain a response into a JSON value (or `Null` when the body is
/// empty, e.g. 204 No Content).
async fn body_to_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

/// PUT helper for the configure routes.
fn put_req(key: &str, value: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/config/{key}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({"value": value}).to_string()))
        .unwrap()
}

/// GET helper for one key.
fn get_req(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// DELETE helper for one key.
fn delete_req(key: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/config/{key}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn put_string_then_get_round_trips() {
    let (app, _h) = setup_app().await;

    let resp = app
        .clone()
        .oneshot(put_req(
            "routing.default_zone",
            json!("weavers.engineering"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get_req("/config/routing.default_zone"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["key"], "routing.default_zone");
    assert_eq!(body["value"], "weavers.engineering");
    assert_eq!(body["source"], "set");
    assert_eq!(body["is_set"], true);
    assert_eq!(body["type"], "string");
}

#[tokio::test]
async fn put_secret_routes_to_secrets_store_and_get_returns_redacted() {
    let (app, handles) = setup_app().await;

    let resp = app
        .clone()
        .oneshot(put_req("cloudflare.api_token", json!("cf-secret-xyz")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Sanity: the secrets store actually has the value (proves routing).
    let bytes = handles.secrets.fetch("cloudflare.api_token").await.unwrap();
    assert_eq!(bytes, b"cf-secret-xyz");

    // GET without ?show_secret returns redacted placeholder.
    let resp = app
        .clone()
        .oneshot(get_req("/config/cloudflare.api_token"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["value"], "<redacted>");
    assert_eq!(body["source"], "set");
    assert_eq!(body["type"], "secret");
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        !raw.contains("cf-secret-xyz"),
        "redacted response must not include the value: {raw}"
    );
}

#[tokio::test]
async fn put_secret_with_show_secret_query_returns_value() {
    let (app, _h) = setup_app().await;
    app.clone()
        .oneshot(put_req("cloudflare.api_token", json!("cf-shown-value")))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(get_req("/config/cloudflare.api_token?show_secret=1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["value"], "cf-shown-value");
    assert_eq!(body["source"], "set");
}

#[tokio::test]
async fn put_unknown_key_returns_400_with_did_you_mean_in_body() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(put_req("cloudflrae.api_token", json!("anything")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_to_json(resp).await;
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("did you mean"), "error: {err}");
    assert!(err.contains("cloudflare.api_token"), "error: {err}");
}

#[tokio::test]
async fn put_invalid_type_returns_400() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(put_req("ssh.max_ttl_seconds", json!("not-an-int")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_to_json(resp).await;
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("invalid value"), "error: {err}");
}

#[tokio::test]
async fn delete_unset_key_returns_404() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(delete_req("routing.default_zone"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_set_key_returns_204_and_subsequent_get_returns_default_or_404() {
    let (app, _h) = setup_app().await;

    // `acme.directory` has a default; `routing.default_zone` does not.
    // Set then delete both, confirm both branches.
    app.clone()
        .oneshot(put_req(
            "acme.directory",
            json!("https://acme-staging-v02.api.letsencrypt.org/directory"),
        ))
        .await
        .unwrap();
    app.clone()
        .oneshot(put_req(
            "routing.default_zone",
            json!("weavers.engineering"),
        ))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(delete_req("acme.directory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // acme.directory has a default; GET falls back to it.
    let resp = app
        .clone()
        .oneshot(get_req("/config/acme.directory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["source"], "default");
    assert_eq!(body["is_set"], false);
    assert_eq!(
        body["value"],
        "https://acme-v02.api.letsencrypt.org/directory"
    );

    let resp = app
        .clone()
        .oneshot(delete_req("routing.default_zone"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // routing.default_zone has no default; GET 404s.
    let resp = app
        .clone()
        .oneshot(get_req("/config/routing.default_zone"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_unset_key_with_default_returns_default_marker_in_source() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(get_req("/config/acme.directory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    assert_eq!(body["source"], "default");
    assert_eq!(body["is_set"], false);
    assert_eq!(
        body["value"],
        "https://acme-v02.api.letsencrypt.org/directory"
    );
}

#[tokio::test]
async fn get_unset_key_without_default_returns_404() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(get_req("/config/acme.contact_email"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_returns_all_six_v01_keys_secrets_redacted_by_default() {
    let (app, _h) = setup_app().await;

    // Set the secret so list has something to redact.
    app.clone()
        .oneshot(put_req("cloudflare.api_token", json!("must-not-leak")))
        .await
        .unwrap();

    let resp = app.clone().oneshot(get_req("/config")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    let arr = body.as_array().expect("list returns an array");
    assert_eq!(arr.len(), 6, "v0.1 schema declares six keys: {arr:?}");

    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        !raw.contains("must-not-leak"),
        "list must not echo secret values by default: {raw}"
    );

    // Spot-check: cloudflare.api_token is "set" + value redacted.
    let cf = arr
        .iter()
        .find(|r| r["key"] == "cloudflare.api_token")
        .expect("cloudflare.api_token present");
    assert_eq!(cf["source"], "set");
    assert_eq!(cf["value"], "<redacted>");

    // acme.directory uses its default.
    let dir = arr
        .iter()
        .find(|r| r["key"] == "acme.directory")
        .expect("acme.directory present");
    assert_eq!(dir["source"], "default");
    assert_eq!(
        dir["value"],
        "https://acme-v02.api.letsencrypt.org/directory"
    );
}

#[tokio::test]
async fn schema_endpoint_returns_all_six_v01_keys() {
    let (app, _h) = setup_app().await;
    let resp = app
        .clone()
        .oneshot(get_req("/config/schema"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp).await;
    let arr = body.as_array().expect("schema returns an array");
    assert_eq!(arr.len(), 6, "v0.1 schema declares six keys");
    let keys: Vec<&str> = arr.iter().filter_map(|e| e["key"].as_str()).collect();
    assert!(keys.contains(&"cloudflare.api_token"));
    assert!(keys.contains(&"cloudflare.zone_id"));
    assert!(keys.contains(&"routing.default_zone"));
    assert!(keys.contains(&"acme.contact_email"));
    assert!(keys.contains(&"acme.directory"));
    assert!(keys.contains(&"ssh.max_ttl_seconds"));
}
