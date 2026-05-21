//! Integration test for `GET /api/v1/ssh/ca`. Verifies the dashboard
//! exposes the controller's SSH CA pubkey in OpenSSH wire format so
//! operators can drop it into a non-Isengard host's `TrustedUserCAKeys`
//! file by piping the response through `isd ssh ca pubkey`.

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
    });
    let app = api::router(handles.clone());
    (app, handles)
}

#[tokio::test]
async fn ssh_ca_returns_openssh_pubkey() {
    let (app, handles) = setup_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/ssh/ca")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let pubkey = body
        .get("pubkey")
        .and_then(|v| v.as_str())
        .expect("response carries a pubkey field");
    // OpenSSH wire format always begins with a key type prefix; the
    // SshAuthority for tests uses Ed25519.
    assert!(
        pubkey.starts_with("ssh-ed25519 "),
        "pubkey should be OpenSSH ed25519 format, got: {pubkey:?}"
    );
    // Matches the bytes the SshAuthority publishes directly so the
    // CLI rendering and the agent's `TrustedUserCAKeys` drop-in agree.
    let expected = String::from_utf8_lossy(handles.ssh_ca.public_key_openssh()).to_string();
    assert_eq!(pubkey.trim_end(), expected.trim_end());
}
