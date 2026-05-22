//! Integration tests for the `/api/v1/hosts/{id}` PATCH endpoint.
//!
//! PR B of the `isd ssh hosts` UX overhaul wires `dial_target` as the
//! one patchable host field. `isd init` / `isd join` POSTs the
//! operator's docker context URL after enrollment; `isd ssh hosts set
//! <agent> --dial <target>` overrides it. Both routes use this PATCH
//! endpoint.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::api;
use isengard_storage::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use tower::ServiceExt;

async fn setup_app() -> (axum::Router, Arc<ControllerHandles>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    let handles = Arc::new(ControllerHandles {
        inventory: inv.clone(),
        journal,
        bus,
        routing,
        enrollment,
        revocation,
        db_path: std::path::PathBuf::from(":memory:"),
        log_fanout: isengard_controller::log_fanout::LogFanout::new(),
        compose_broker: Arc::new(isengard_controller::compose_broker::ComposeBroker::new()),
        secrets: Arc::new(isengard_controller::secrets::SecretsStore::new_locked(
            inv.clone(),
        )),
        ca,
        ssh_ca: Arc::new(isengard_controller::ssh_ca::SshAuthority::for_tests().unwrap()),
        config_dispatcher: ControllerHandles::test_config_dispatcher(
            inv.clone(),
            Arc::new(isengard_controller::secrets::SecretsStore::new_locked(
                inv.clone(),
            )),
        ),
    });
    let app = api::router(handles.clone());
    (app, handles)
}

async fn seed_host(inv: &Inventory) -> String {
    let id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-test".into(),
            hostname: "edge-1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.7.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();
    ulid::Ulid::from(id).to_string()
}

/// Happy path: PATCH /hosts/{id} with a `dial_target` body sets the
/// stored value and the GET response (and the PATCH response) carries
/// the new value.
#[tokio::test]
async fn patch_host_sets_dial_target() {
    let (app, handles) = setup_app().await;
    let host_id = seed_host(&handles.inventory).await;

    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dial_target":"dirdmaster@10.17.0.125"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed.get("dial_target").and_then(|v| v.as_str()),
        Some("dirdmaster@10.17.0.125")
    );

    // GET reflects the same value.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/hosts/{host_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed.get("dial_target").and_then(|v| v.as_str()),
        Some("dirdmaster@10.17.0.125")
    );
}

/// Empty string clears the stored value back to NULL so the CLI shows
/// `(unset)` again.
#[tokio::test]
async fn patch_host_empty_dial_target_clears() {
    let (app, handles) = setup_app().await;
    let host_id = seed_host(&handles.inventory).await;

    // Set first.
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dial_target":"op@host"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Clear with an empty string.
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dial_target":""}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        parsed
            .get("dial_target")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "dial_target should be null after empty-string clear: {parsed}"
    );
}

/// Omitting the field leaves the row unchanged so a future PATCH that
/// modifies a different field never accidentally clears the dial
/// target.
#[tokio::test]
async fn patch_host_omitted_field_is_noop() {
    let (app, handles) = setup_app().await;
    let host_id = seed_host(&handles.inventory).await;

    // Set first.
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dial_target":"keep-me"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now PATCH with an empty body: the dial_target should stay.
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed.get("dial_target").and_then(|v| v.as_str()),
        Some("keep-me")
    );
}

/// 404 when the host id is well-formed but no row matches.
#[tokio::test]
async fn patch_host_unknown_id_returns_404() {
    let (app, _handles) = setup_app().await;
    let ghost = ulid::Ulid::new().to_string();
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{ghost}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dial_target":"x@y"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 400 when the body itself is unparseable (malformed JSON).
#[tokio::test]
async fn patch_host_malformed_body_returns_400() {
    let (app, handles) = setup_app().await;
    let host_id = seed_host(&handles.inventory).await;
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/hosts/{host_id}"))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // axum returns 400/422 for malformed JSON; both are acceptable here
    // (the contract is "not 200, not 500").
    let s = resp.status();
    assert!(
        s == StatusCode::BAD_REQUEST || s == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 4xx for malformed body, got {s}"
    );
}
