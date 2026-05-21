//! REST endpoint coverage for `/api/v1/deployment-groups` + the per-stack
//! parallelism setter. (T3, refs #50).

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
use isengard_plugin_dashboard::deployment_groups::{
    DeploymentGroupDetailDto, DeploymentGroupDto, ParallelismDto,
};
use isengard_plugin_dashboard::deployments::{self, DeploymentDto};
use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::host::{EnrollHost, HostId};
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use isengard_storage::stack::{InsertStack, StackId, StackSource};
use isengard_storage::{DeploymentGroupState, InsertDeploymentGroup};
use tower::ServiceExt;
use ulid::Ulid;

async fn setup_app() -> (Router, Router, Arc<Inventory>, HostId, StackId) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-grp".into(),
            hostname: "h-grp".into(),
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
        ca,
        ssh_ca: Arc::new(isengard_controller::ssh_ca::SshAuthority::for_tests().unwrap()),
        config_dispatcher: ControllerHandles::test_config_dispatcher(
            inv.clone(),
            Arc::new(isengard_controller::secrets::SecretsStore::new_locked(
                inv.clone(),
            )),
        ),
    });

    let groups_app = isengard_plugin_dashboard::deployment_groups::router(handles.clone());
    let deployments_app = deployments::router(handles);
    (groups_app, deployments_app, inv, host_id, stack_id)
}

async fn enroll_extra(inv: &Inventory, name: &str) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: format!("fp-{name}"),
        hostname: name.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0".into(),
        docker_version: "27.0".into(),
    })
    .await
    .unwrap()
}

fn dep(host: HostId, stack: StackId, state: DeploymentState) -> InsertDeployment {
    InsertDeployment {
        id: Ulid::new().to_string(),
        host_id: host,
        stack_id: stack,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state,
        blue_container: None,
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: None,
        health_path: None,
        container_port: None,
        metadata_json: None,
        previous_digest: None,
    }
}

async fn body_json<T: serde::de::DeserializeOwned>(resp: axum::response::Response) -> T {
    let bytes = axum::body::to_bytes(resp.into_body(), 5_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        let s = String::from_utf8_lossy(&bytes);
        panic!("decode failed: {e}; body: {s}")
    })
}

#[tokio::test]
async fn list_groups_for_stack_returns_inserted_row() {
    let (app, _, inv, host, stack) = setup_app().await;
    let h2 = enroll_extra(&inv, "h2").await;
    let g = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Rolling,
            target_hosts: vec![host, h2],
        })
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!("/deployment-groups?stack_id={}", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dtos: Vec<DeploymentGroupDto> = body_json(resp).await;
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, g.id);
    assert_eq!(dtos[0].state, "rolling");
    assert_eq!(dtos[0].target_hosts.len(), 2);
}

#[tokio::test]
async fn list_groups_state_filter_active_drops_terminal() {
    let (app, _, inv, host, stack) = setup_app().await;
    let g_active = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Rolling,
            target_hosts: vec![host],
        })
        .await
        .unwrap();
    let g_done = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Pending,
            target_hosts: vec![host],
        })
        .await
        .unwrap();
    inv.update_deployment_group_state(&g_done.id, DeploymentGroupState::Done, None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!(
            "/deployment-groups?stack_id={}&state=active",
            stack.0
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dtos: Vec<DeploymentGroupDto> = body_json(resp).await;
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, g_active.id);
}

#[tokio::test]
async fn list_groups_state_filter_failed_only_returns_failed() {
    let (app, _, inv, host, stack) = setup_app().await;
    let g_fail = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Pending,
            target_hosts: vec![host],
        })
        .await
        .unwrap();
    inv.update_deployment_group_state(&g_fail.id, DeploymentGroupState::Failed, Some("boom"))
        .await
        .unwrap();
    inv.insert_deployment_group(InsertDeploymentGroup {
        id: Ulid::new().to_string(),
        stack_id: stack,
        service_name: "web".into(),
        parallelism: "1".into(),
        state: DeploymentGroupState::Rolling,
        target_hosts: vec![host],
    })
    .await
    .unwrap();

    let req = Request::builder()
        .uri(format!(
            "/deployment-groups?stack_id={}&state=failed",
            stack.0
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dtos: Vec<DeploymentGroupDto> = body_json(resp).await;
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, g_fail.id);
    assert_eq!(dtos[0].error.as_deref(), Some("boom"));
}

#[tokio::test]
async fn get_single_group_returns_embedded_deployments() {
    let (app, _, inv, host, stack) = setup_app().await;
    let g = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Rolling,
            target_hosts: vec![host],
        })
        .await
        .unwrap();
    let d = inv
        .insert_deployment(dep(host, stack, DeploymentState::SpinningUp))
        .await
        .unwrap();
    inv.set_deployment_group(&d.id, &g.id).await.unwrap();

    let req = Request::builder()
        .uri(format!("/deployment-groups/{}", g.id))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: DeploymentGroupDetailDto = body_json(resp).await;
    assert_eq!(dto.group.id, g.id);
    assert_eq!(dto.deployments.len(), 1);
    assert_eq!(dto.deployments[0].id, d.id);
}

#[tokio::test]
async fn get_single_group_returns_404_for_unknown_id() {
    let (app, _, _, _, _) = setup_app().await;
    let req = Request::builder()
        .uri("/deployment-groups/missing-id")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_group_marks_aborted_when_active() {
    let (app, _, inv, host, stack) = setup_app().await;
    let g = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Rolling,
            target_hosts: vec![host],
        })
        .await
        .unwrap();

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/deployment-groups/{}", g.id))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let after = inv.get_deployment_group(&g.id).await.unwrap().unwrap();
    assert_eq!(after.state, DeploymentGroupState::Aborted);
}

#[tokio::test]
async fn delete_group_is_noop_when_already_terminal() {
    let (app, _, inv, host, stack) = setup_app().await;
    let g = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Pending,
            target_hosts: vec![host],
        })
        .await
        .unwrap();
    inv.update_deployment_group_state(&g.id, DeploymentGroupState::Done, None)
        .await
        .unwrap();

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/deployment-groups/{}", g.id))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after = inv.get_deployment_group(&g.id).await.unwrap().unwrap();
    assert_eq!(after.state, DeploymentGroupState::Done);
}

#[tokio::test]
async fn set_parallelism_persists_value() {
    let (app, _, inv, _host, stack) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/stacks/{}/deployment-parallelism", stack.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"parallelism":"2"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let stored = inv.get_stack_parallelism(stack).await.unwrap();
    assert_eq!(stored.as_deref(), Some("2"));
}

#[tokio::test]
async fn set_parallelism_accepts_all_sentinel() {
    let (app, _, inv, _host, stack) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/stacks/{}/deployment-parallelism", stack.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"parallelism":"all"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        inv.get_stack_parallelism(stack).await.unwrap().as_deref(),
        Some("all")
    );
}

#[tokio::test]
async fn set_parallelism_rejects_garbage() {
    let (app, _, _, _host, stack) = setup_app().await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/stacks/{}/deployment-parallelism", stack.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"parallelism":"banana"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_parallelism_clears_when_null() {
    let (app, _, inv, _host, stack) = setup_app().await;
    inv.set_stack_parallelism(stack, Some("3")).await.unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/stacks/{}/deployment-parallelism", stack.0))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"parallelism":null}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(inv.get_stack_parallelism(stack).await.unwrap().is_none());
}

#[tokio::test]
async fn get_parallelism_returns_persisted_value() {
    let (app, _, inv, _host, stack) = setup_app().await;
    inv.set_stack_parallelism(stack, Some("all")).await.unwrap();
    let req = Request::builder()
        .uri(format!("/stacks/{}/deployment-parallelism", stack.0))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: ParallelismDto = body_json(resp).await;
    assert_eq!(dto.stack_id, stack.0);
    assert_eq!(dto.parallelism.as_deref(), Some("all"));
}

#[tokio::test]
async fn list_deployments_with_group_id_filter_returns_grouped_only() {
    let (_, deployments_app, inv, host, stack) = setup_app().await;
    let g = inv
        .insert_deployment_group(InsertDeploymentGroup {
            id: Ulid::new().to_string(),
            stack_id: stack,
            service_name: "web".into(),
            parallelism: "1".into(),
            state: DeploymentGroupState::Rolling,
            target_hosts: vec![host],
        })
        .await
        .unwrap();

    let in_group = inv
        .insert_deployment(dep(host, stack, DeploymentState::SpinningUp))
        .await
        .unwrap();
    inv.set_deployment_group(&in_group.id, &g.id).await.unwrap();
    let _outside_group = inv
        .insert_deployment(dep(host, stack, DeploymentState::SpinningUp))
        .await
        .unwrap();

    let req = Request::builder()
        .uri(format!("/deployments?group_id={}", g.id))
        .body(Body::empty())
        .unwrap();
    let resp = deployments_app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dtos: Vec<DeploymentDto> = body_json(resp).await;
    assert_eq!(dtos.len(), 1);
    assert_eq!(dtos[0].id, in_group.id);
}
