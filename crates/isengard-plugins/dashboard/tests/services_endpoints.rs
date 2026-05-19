//! Integration tests for `/api/v1/services/:stack_id/:service_name`
//! Mirrors the harness used by `policies_endpoints.rs`.
//!
//! Builds the dashboard API router against an in-memory `Inventory` +
//! `Journal`, seeds the relevant rows, and asserts the JSON envelope shape
//! returned for the service detail page.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_core::policy::{Policy, PolicyScopeType, UpdateGate};
use isengard_plugin_dashboard::api;
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::{InsertEvent, Journal};
use isengard_storage::{
    EnrollHost, HostId, InsertPolicy, InsertRoutingRule, InsertService, InsertStack,
    RoutingRuleSource, RoutingRuleState, ServiceState, StackId, StackSource, TlsMode,
};
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
    });
    let app = api::router(handles.clone());
    (app, handles)
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

async fn enroll(handles: &ControllerHandles, hostname: &str, fleet: &str) -> HostId {
    let host_id = handles
        .inventory
        .enroll_host(EnrollHost {
            fingerprint: format!("fp-{hostname}"),
            hostname: hostname.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap();
    // kill-fleets: the `fleet` column is gone; tests that exercise fleet-
    // scoped policy resolution write the value as a host label so the
    // resolver still sees it via `list_agent_labels`.
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("fleet".to_string(), fleet.to_string());
    handles
        .inventory
        .replace_agent_labels(host_id, &labels)
        .await
        .unwrap();
    host_id
}

async fn seed_stack(
    handles: &ControllerHandles,
    host_id: HostId,
    stack_name: &str,
    service_name: &str,
    image: &str,
) -> (StackId, isengard_storage::ServiceId) {
    let stack_id = handles
        .inventory
        .insert_stack(InsertStack {
            host_id,
            name: stack_name.into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();
    let svc_id = handles
        .inventory
        .insert_service(InsertService {
            host_id,
            stack_id: Some(stack_id),
            name: service_name.into(),
            image: image.into(),
            state: ServiceState::Running,
        })
        .await
        .unwrap();
    (stack_id, svc_id)
}

#[tokio::test]
async fn service_detail_returns_404_when_stack_missing() {
    let (app, _h) = setup_app().await;
    let resp = app.oneshot(get_req("/services/9999/web")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn service_detail_returns_404_when_service_missing() {
    let (app, handles) = setup_app().await;
    let host_id = enroll(&handles, "h1", "default").await;
    let stack_id = handles
        .inventory
        .insert_stack(InsertStack {
            host_id,
            name: "blog".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn service_detail_single_host_returns_envelope() {
    let (app, handles) = setup_app().await;
    let host_id = enroll(&handles, "h1", "default").await;
    let (stack_id, _svc_id) =
        seed_stack(&handles, host_id, "blog", "web", "ghcr.io/foo/web:1.0.0").await;

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;

    assert_eq!(v["service"]["name"], "web");
    assert_eq!(v["service"]["image"], "ghcr.io/foo/web:1.0.0");
    assert_eq!(v["service"]["state"], "running");
    assert_eq!(v["service"]["hostname"], "h1");
    assert!(v["other_instances"].is_array());
    assert_eq!(v["other_instances"].as_array().unwrap().len(), 0);
    assert_eq!(v["recent_events"].as_array().unwrap().len(), 0);
    assert_eq!(v["routing_rules"].as_array().unwrap().len(), 0);
    // Last deployment is None when no deployment row has been created.
    assert!(v["last_deployment"].is_null());
    // Effective policy carries the Default-rooted resolution even with
    // no policy rows configured.
    assert!(v["effective_policy"]["strategy"].is_string());
    assert!(v["effective_policy"]["gate"].is_string());
}

#[tokio::test]
async fn service_detail_includes_effective_policy() {
    let (app, handles) = setup_app().await;
    let host_id = enroll(&handles, "h1", "prod").await;
    let (stack_id, _svc_id) = seed_stack(&handles, host_id, "blog", "web", "img:1.0").await;

    // Set a fleet-level policy that overrides the default gate.
    let policy = Policy {
        gate: Some(UpdateGate::Approval),
        ..Policy::default()
    };
    handles
        .inventory
        .insert_policy(InsertPolicy {
            scope_type: PolicyScopeType::Fleet,
            scope_key: "prod".into(),
            body: policy,
        })
        .await
        .unwrap();

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["effective_policy"]["gate"], "approval");
    // Provenance: the gate came from the FLEET row.
    assert_eq!(v["effective_policy"]["provenance"]["gate"], "fleet");
}

#[tokio::test]
async fn service_detail_multi_host_lists_other_instances() {
    let (app, handles) = setup_app().await;
    let h1 = enroll(&handles, "h1", "prod").await;
    let h2 = enroll(&handles, "h2", "prod").await;
    let h3 = enroll(&handles, "h3", "prod").await;

    let (stack_id_1, _) = seed_stack(&handles, h1, "blog", "web", "img:1.0").await;
    let (_stack_id_2, _) = seed_stack(&handles, h2, "blog", "web", "img:1.0").await;
    let (_stack_id_3, _) = seed_stack(&handles, h3, "blog", "web", "img:1.0").await;
    // Different stack name on a fourth host: must NOT show up.
    let h4 = enroll(&handles, "h4", "prod").await;
    let (_, _) = seed_stack(&handles, h4, "wiki", "web", "img:1.0").await;

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id_1.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;

    let others = v["other_instances"].as_array().unwrap();
    assert_eq!(others.len(), 2, "expected h2 + h3, got {others:?}");
    let mut hostnames: Vec<&str> = others
        .iter()
        .map(|o| o["hostname"].as_str().unwrap())
        .collect();
    hostnames.sort();
    assert_eq!(hostnames, vec!["h2", "h3"]);
}

#[tokio::test]
async fn service_detail_includes_attached_routing_rules() {
    let (app, handles) = setup_app().await;
    let host_id = enroll(&handles, "h1", "default").await;
    let (stack_id, _svc_id) = seed_stack(&handles, host_id, "blog", "web", "img:1.0").await;

    handles
        .inventory
        .insert_routing_rule(InsertRoutingRule {
            host_id,
            stack_id: Some(stack_id),
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: "blog.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: Some("/healthz".into()),
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    // A second rule for a different service must NOT show up.
    handles
        .inventory
        .insert_routing_rule(InsertRoutingRule {
            host_id,
            stack_id: Some(stack_id),
            service_name: "api".into(),
            container_port: 9090,
            public_hostname: "api.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let rules = v["routing_rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["public_hostname"], "blog.example.com");
    assert_eq!(rules[0]["service_name"], "web");
}

#[tokio::test]
async fn service_detail_filters_recent_events_to_this_service() {
    let (app, handles) = setup_app().await;
    let host_id = enroll(&handles, "h1", "default").await;
    let (stack_id, _svc_id) = seed_stack(&handles, host_id, "blog", "web", "img:1.0").await;

    handles
        .journal
        .insert(InsertEvent {
            kind: "update.success".into(),
            host_id: Some(host_id),
            container_name: Some("web".into()),
            image: Some("img:1.0".into()),
            old_digest: None,
            new_digest: None,
            error: None,
            summary: "updated web to img:1.0".into(),
            metadata_json: None,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();

    // Event for a different container on the same host: filtered out.
    handles
        .journal
        .insert(InsertEvent {
            kind: "update.success".into(),
            host_id: Some(host_id),
            container_name: Some("other".into()),
            image: Some("other:1.0".into()),
            old_digest: None,
            new_digest: None,
            error: None,
            summary: "updated other".into(),
            metadata_json: None,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();

    let resp = app
        .oneshot(get_req(&format!("/services/{}/web", stack_id.0)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let events = v["recent_events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["container_name"], "web");
}
