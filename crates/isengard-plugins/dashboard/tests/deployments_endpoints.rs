//! Integration tests for `GET /api/v1/deployments` (Phase 10 Plan B Task 5).
//!
//! Builds the deployments router against an in-memory `Inventory` and verifies
//! that the `state=active` filter excludes terminal rows and `state=history`
//! includes only terminal rows.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::deployments::{self, AbortResponse, DeploymentDto};
use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::host::{EnrollHost, HostId};
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use isengard_storage::service::{InsertService, ServiceState};
use isengard_storage::stack::{InsertStack, StackId, StackSource};
use tower::ServiceExt;
use ulid::Ulid;

async fn setup_app() -> (axum::Router, Arc<Inventory>, HostId, StackId) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-deploy".into(),
            hostname: "h-deploy".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();

    let stack_id = inv
        .insert_stack(InsertStack {
            host_id,
            name: "blog".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

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
    });

    let app = deployments::router(handles);
    (app, inv, host_id, stack_id)
}

fn sample_dep(host_id: HostId, stack_id: StackId, state: DeploymentState) -> InsertDeployment {
    InsertDeployment {
        id: Ulid::new().to_string(),
        host_id,
        stack_id,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state,
        blue_container: Some("c-blue".into()),
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/".into()),
        container_port: Some(80),
        metadata_json: None,
        previous_digest: None,
    }
}

#[tokio::test]
async fn list_active_returns_only_non_terminal() {
    let (app, inv, host, stack) = setup_app().await;

    // Active row: stays in non-terminal state.
    inv.insert_deployment(sample_dep(host, stack, DeploymentState::SpinningUp))
        .await
        .unwrap();

    // Terminal row: insert pending, then transition to Done.
    let done = inv
        .insert_deployment(sample_dep(host, stack, DeploymentState::Pending))
        .await
        .unwrap();
    inv.update_deployment_state(&done.id, DeploymentState::Done)
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?stack_id={}&state=active", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let dtos: Vec<DeploymentDto> = serde_json::from_slice(&body).unwrap();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].state, "spinning_up");
}

#[tokio::test]
async fn list_history_returns_only_terminal() {
    let (app, inv, host, stack) = setup_app().await;

    inv.insert_deployment(sample_dep(host, stack, DeploymentState::SpinningUp))
        .await
        .unwrap();
    let done = inv
        .insert_deployment(sample_dep(host, stack, DeploymentState::Pending))
        .await
        .unwrap();
    inv.update_deployment_state(&done.id, DeploymentState::Done)
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?stack_id={}&state=history", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let dtos: Vec<DeploymentDto> = serde_json::from_slice(&body).unwrap();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].state, "done");
}

#[tokio::test]
async fn abort_returns_noop_for_terminal_deployment() {
    let (app, inv, host, stack) = setup_app().await;
    let dep = inv
        .insert_deployment(sample_dep(host, stack, DeploymentState::Pending))
        .await
        .unwrap();
    inv.update_deployment_state(&dep.id, DeploymentState::Done)
        .await
        .unwrap();

    let req = Request::builder()
        .method("POST")
        .uri(format!("/deployments/{}/abort", dep.id))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let r: AbortResponse = serde_json::from_slice(&body).unwrap();
    assert!(r.noop);
    assert!(
        r.reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("deployment_already_terminal:")
    );
}

#[tokio::test]
async fn abort_returns_404_for_unknown_deployment() {
    let (app, _inv, _host, _stack) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/deployments/unknown-id/abort")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_service_strategy_persists() {
    let (app, inv, host, stack) = setup_app().await;
    let svc_id = inv
        .insert_service(InsertService {
            host_id: host,
            stack_id: Some(stack),
            name: "web".into(),
            image: "nginx:alpine".into(),
            state: ServiceState::Running,
        })
        .await
        .unwrap();

    let req = Request::builder()
        .method("PUT")
        .uri(format!("/services/{}/deploy-strategy", svc_id.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"override_value":"blue-green"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after = inv.get_service(svc_id).await.unwrap().unwrap();
    assert_eq!(
        after.deploy_strategy_override.as_deref(),
        Some("blue-green")
    );
}

#[tokio::test]
async fn put_service_strategy_rejects_unknown_value() {
    let (app, _inv, _host, _stack) = setup_app().await;
    let req = Request::builder()
        .method("PUT")
        .uri("/services/1/deploy-strategy")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"override_value":"bogus"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Phase 9F (#48): the deployments DTO surfaces previous_digest +
/// rollback_attempted_at when set. Insert a row with both fields
/// populated, GET it, assert the JSON carries the values.
#[tokio::test]
async fn list_active_surfaces_rollback_fields() {
    let (app, inv, host, stack) = setup_app().await;

    let mut ins = sample_dep(host, stack, DeploymentState::SpinningUp);
    ins.previous_digest = Some("sha256:before".into());
    let inserted = inv.insert_deployment(ins).await.unwrap();
    let when = chrono::Utc::now();
    inv.set_deployment_rollback_attempted(&inserted.id, when)
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?stack_id={}&state=active", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let dtos: Vec<DeploymentDto> = serde_json::from_slice(&body).unwrap();
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].previous_digest.as_deref(), Some("sha256:before"));
    assert!(
        dtos[0].rollback_attempted_at.is_some(),
        "rollback_attempted_at should be serialized"
    );
}
