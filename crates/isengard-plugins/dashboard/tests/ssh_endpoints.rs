//! Integration tests for the SSH bastion dashboard surface:
//!  - `GET /api/v1/ssh/ca`: CA pubkey introspection.
//!  - `GET /api/v1/ssh/audit`: `ssh.cert.*` journal slice (Phase 6).
//!
//! Both tests build a real `ControllerHandles` over in-memory storage
//! and exercise the router through `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::api;
use isengard_storage::InsertEvent;
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
            Arc::new(isengard_controller::secrets::SecretsStore::new_locked(inv.clone())),
        ),
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

/// Seed three `ssh.cert.issued` events plus one unrelated
/// `container.started` event in the fixture journal. The audit
/// endpoint must keep only the SSH rows.
async fn seed_audit_fixture(handles: &Arc<ControllerHandles>) -> [chrono::DateTime<Utc>; 3] {
    let t0 = Utc::now() - Duration::minutes(30);
    let t1 = Utc::now() - Duration::minutes(15);
    let t2 = Utc::now() - Duration::minutes(2);
    for (i, ts) in [t0, t1, t2].iter().enumerate() {
        let meta = serde_json::json!({
            "pubkey_fingerprint": format!("SHA256:fixture{i}"),
            "principals": ["isengard"],
            "ttl_seconds": 3600,
            "comment": format!("operator@laptop fixture-{i}"),
        })
        .to_string();
        handles
            .journal
            .insert(InsertEvent {
                host_id: None,
                kind: "ssh.cert.issued".into(),
                container_name: None,
                image: None,
                old_digest: None,
                new_digest: None,
                error: None,
                summary: format!("ssh cert issued (fixture {i})"),
                metadata_json: Some(meta),
                occurred_at: *ts,
            })
            .await
            .unwrap();
    }
    // Unrelated event: must NOT show up under /ssh/audit.
    handles
        .journal
        .insert(InsertEvent {
            host_id: None,
            kind: "container.started".into(),
            container_name: Some("nginx".into()),
            image: Some("nginx:1.27".into()),
            old_digest: None,
            new_digest: None,
            error: None,
            summary: "container started".into(),
            metadata_json: None,
            occurred_at: Utc::now() - Duration::minutes(10),
        })
        .await
        .unwrap();
    [t0, t1, t2]
}

#[tokio::test]
async fn ssh_audit_filters_to_cert_events() {
    let (app, handles) = setup_app().await;
    let _ = seed_audit_fixture(&handles).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/ssh/audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        entries.len(),
        3,
        "only the three ssh.cert.issued rows should come back: {entries:?}"
    );
    for e in &entries {
        let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            kind.starts_with("ssh.cert."),
            "every row must be ssh.cert.*, got {kind}"
        );
        // Metadata round-trips as a JSON object, not the raw string.
        let meta = e.get("metadata").expect("metadata field present");
        assert!(
            meta.get("pubkey_fingerprint").is_some(),
            "metadata decodes to an object: {meta:?}"
        );
    }
}

#[tokio::test]
async fn ssh_audit_since_cutoff_drops_older_rows() {
    let (app, handles) = setup_app().await;
    let [_t0, t1, _t2] = seed_audit_fixture(&handles).await;
    // Cutoff at t1 keeps t1 (inclusive) and t2; drops t0. The
    // RFC3339 form `to_rfc3339` emits carries `+00:00`, where `+`
    // means "space" in a URL query: percent-encode it so the
    // dashboard sees the right string.
    let since = t1.to_rfc3339().replace('+', "%2B").replace(':', "%3A");
    let uri = format!("/ssh/audit?since={since}");

    let resp = app
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        entries.len(),
        2,
        "since={since} should keep only the two rows at or after t1: {entries:?}"
    );
}
