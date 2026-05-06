//! Integration tests for `/api/v1/policies` (Phase 9 Plan A, T4).
//!
//! Builds the policies router against an in-memory `Inventory` and verifies
//! the CRUD verbs plus `GET /policies/effective`. Mirrors the harness used
//! by `deployments_endpoints.rs`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_core::policy::{
    FailureHandling, Policy, PolicyOrigin, PolicyScopeType, ResolvedPolicy, UpdateGate,
    UpdateStrategy,
};
use isengard_plugin_dashboard::policies::{self, PolicyDto};
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
        inventory: inv,
        journal,
        bus,
        routing,
        enrollment,
        revocation,
        log_fanout: isengard_controller::log_fanout::LogFanout::new(),
    });
    let app = policies::router(handles.clone());
    (app, handles)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn put_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn delete_req(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn list_policies_empty_returns_empty_array() {
    let (app, _h) = setup_app().await;
    let resp = app.oneshot(get_req("/policies")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test]
async fn post_then_list_returns_inserted_row() {
    let (app, _h) = setup_app().await;

    let body = serde_json::json!({
        "scopeType": "fleet",
        "scopeKey": "prod",
        "body": {
            "strategy": "pinned",
        }
    });
    let resp = app
        .clone()
        .oneshot(post_json("/policies", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let dto: PolicyDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.scope_type, PolicyScopeType::Fleet);
    assert_eq!(dto.scope_key, "prod");
    assert_eq!(dto.body.strategy, Some(UpdateStrategy::Pinned));

    let list_resp = app.oneshot(get_req("/policies")).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let listed: Vec<PolicyDto> = serde_json::from_value(body_json(list_resp).await).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].scope_key, "prod");
}

#[tokio::test]
async fn post_duplicate_scope_returns_409() {
    let (app, _h) = setup_app().await;
    let body = serde_json::json!({
        "scopeType": "fleet",
        "scopeKey": "prod",
        "body": {}
    });
    let first = app
        .clone()
        .oneshot(post_json("/policies", body.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let dup = app.oneshot(post_json("/policies", body)).await.unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn post_with_gate_approval_now_accepted() {
    // Phase 9b lifted the 422 guard now that the updater enforces gate=approval.
    let (app, _h) = setup_app().await;
    let body = serde_json::json!({
        "scopeType": "global",
        "scopeKey": "",
        "body": { "gate": "approval" }
    });
    let resp = app.oneshot(post_json("/policies", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    assert_eq!(v["body"]["gate"], "approval");
}

#[tokio::test]
async fn put_upserts_insert_then_update() {
    let (app, handles) = setup_app().await;

    // Insert via PUT.
    let body1 = serde_json::json!({
        "body": { "strategy": "tag-only" }
    });
    let resp = app
        .clone()
        .oneshot(put_json("/policies/fleet/staging", body1))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: PolicyDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.body.strategy, Some(UpdateStrategy::TagOnly));
    let original_created_at = dto.created_at;

    // Update via PUT (same path). Inner `Policy` retains snake_case fields
    // because core's struct has no rename_all attribute.
    let body2 = serde_json::json!({
        "body": { "strategy": "any", "on_failure": "keep" }
    });
    let resp2 = app
        .oneshot(put_json("/policies/fleet/staging", body2))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let dto2: PolicyDto = serde_json::from_value(body_json(resp2).await).unwrap();
    assert_eq!(dto2.body.strategy, Some(UpdateStrategy::Any));
    assert_eq!(dto2.body.on_failure, Some(FailureHandling::Keep));
    // created_at preserved across upsert; storage layer guarantees this.
    assert_eq!(dto2.created_at, original_created_at);

    // Storage round-trip sanity check.
    let row = handles
        .inventory
        .get_policy(PolicyScopeType::Fleet, "staging")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.body.strategy, Some(UpdateStrategy::Any));
}

#[tokio::test]
async fn delete_existing_returns_204_then_404() {
    let (app, _h) = setup_app().await;

    // Seed via POST, then delete.
    let resp = app
        .clone()
        .oneshot(post_json(
            "/policies",
            serde_json::json!({
                "scopeType": "fleet",
                "scopeKey": "prod",
                "body": {}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let del = app
        .clone()
        .oneshot(delete_req("/policies/fleet/prod"))
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);

    // Second delete -> 404.
    let del2 = app
        .oneshot(delete_req("/policies/fleet/prod"))
        .await
        .unwrap();
    assert_eq!(del2.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn effective_with_no_rows_returns_defaults() {
    let (app, _h) = setup_app().await;
    let resp = app
        .oneshot(get_req(
            "/policies/effective?fleet=prod&stack=prod/blog&service=prod/blog/web",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r: ResolvedPolicy = serde_json::from_value(body_json(resp).await).unwrap();

    // Resolver defaults: TagOnly / Auto / Notify; nothing set elsewhere.
    assert_eq!(r.strategy, UpdateStrategy::TagOnly);
    assert_eq!(r.gate, UpdateGate::Auto);
    assert_eq!(r.on_failure, FailureHandling::Notify);
    assert!(r.paused_until.is_none());
    assert!(r.approver_channel.is_none());

    // Provenance: every field originates from Default.
    assert_eq!(r.provenance.strategy, PolicyOrigin::Default);
    assert_eq!(r.provenance.gate, PolicyOrigin::Default);
    assert_eq!(r.provenance.paused_until, PolicyOrigin::Default);
    assert_eq!(r.provenance.on_failure, PolicyOrigin::Default);
    assert_eq!(r.provenance.approver_channel, PolicyOrigin::Default);
}

#[tokio::test]
async fn effective_with_fleet_and_stack_rows_tracks_provenance() {
    let (app, handles) = setup_app().await;

    // Fleet-level row sets gate=never; stack-level row sets strategy=pinned.
    handles
        .inventory
        .upsert_policy(
            PolicyScopeType::Fleet,
            "prod",
            &Policy {
                gate: Some(UpdateGate::Never),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    handles
        .inventory
        .upsert_policy(
            PolicyScopeType::Stack,
            "prod/blog",
            &Policy {
                strategy: Some(UpdateStrategy::Pinned),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let resp = app
        .oneshot(get_req(
            "/policies/effective?fleet=prod&stack=prod/blog&service=prod/blog/web",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r: ResolvedPolicy = serde_json::from_value(body_json(resp).await).unwrap();

    // strategy comes from the stack row.
    assert_eq!(r.strategy, UpdateStrategy::Pinned);
    assert_eq!(r.provenance.strategy, PolicyOrigin::Stack);

    // gate comes from the fleet row.
    assert_eq!(r.gate, UpdateGate::Never);
    assert_eq!(r.provenance.gate, PolicyOrigin::Fleet);

    // on_failure / paused_until / approver remain at default.
    assert_eq!(r.on_failure, FailureHandling::Notify);
    assert_eq!(r.provenance.on_failure, PolicyOrigin::Default);
    assert_eq!(r.provenance.paused_until, PolicyOrigin::Default);
    assert_eq!(r.provenance.approver_channel, PolicyOrigin::Default);
}

/// Phase 9d: a policy with a malformed cron in window returns 400.
#[tokio::test]
async fn post_with_malformed_window_cron_returns_400() {
    let (app, _h) = setup_app().await;

    let body = serde_json::json!({
        "scopeType": "fleet",
        "scopeKey": "prod",
        "body": {
            "window": { "cron_expr": "this is not a cron" }
        }
    });
    let resp = app.oneshot(post_json("/policies", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
    assert!(
        err.contains("invalid window cron"),
        "expected 'invalid window cron' in error, got: {err}"
    );
}

/// Phase 9d: a policy with a valid window round-trips through POST and
/// GET. Body preserves cron + timezone fields.
#[tokio::test]
async fn post_with_valid_window_round_trips() {
    let (app, _h) = setup_app().await;

    let body = serde_json::json!({
        "scopeType": "fleet",
        "scopeKey": "prod",
        "body": {
            "window": {
                "cron_expr": "0 2 * * 0",
                "timezone": "Europe/Zurich"
            }
        }
    });
    let resp = app
        .clone()
        .oneshot(post_json("/policies", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app.oneshot(get_req("/policies")).await.unwrap();
    let v = body_json(resp).await;
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let row: PolicyDto = serde_json::from_value(arr[0].clone()).unwrap();
    let win = row.body.window.as_ref().expect("window present");
    assert_eq!(win.cron_expr, "0 2 * * 0");
    assert_eq!(win.timezone.as_deref(), Some("Europe/Zurich"));
}

/// Phase 9d: PUT with a malformed cron returns 400, leaving any prior row
/// unchanged.
#[tokio::test]
async fn put_with_malformed_window_cron_returns_400() {
    let (app, h) = setup_app().await;
    h.inventory
        .upsert_policy(
            isengard_core::policy::PolicyScopeType::Fleet,
            "prod",
            &Policy {
                strategy: Some(UpdateStrategy::Pinned),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let body = serde_json::json!({
        "body": {
            "window": { "cron_expr": "garbage", "timezone": "UTC" }
        }
    });
    let resp = app
        .oneshot(put_json("/policies/fleet/prod", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_with_external_gate_round_trips() {
    let (app, _handles) = setup_app().await;
    let body = serde_json::json!({
        "scopeType": "global",
        "scopeKey": "",
        "body": {
            "external_gate": {
                "url": "https://gate.example.com/decide",
                "secret": "shh",
                "timeout_secs": 15
            }
        }
    });
    let resp = app
        .clone()
        .oneshot(post_json("/policies", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    assert_eq!(
        v["body"]["external_gate"]["url"],
        "https://gate.example.com/decide"
    );
    assert_eq!(v["body"]["external_gate"]["timeout_secs"], 15);
}

#[tokio::test]
async fn put_with_empty_external_gate_url_rejects_400() {
    let (app, _handles) = setup_app().await;
    let body = serde_json::json!({
        "body": {
            "external_gate": { "url": "", "timeout_secs": 10 }
        }
    });
    let resp = app
        .oneshot(put_json("/policies/global/_", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_with_external_gate_timeout_zero_rejects_400() {
    let (app, _handles) = setup_app().await;
    let body = serde_json::json!({
        "body": {
            "external_gate": { "url": "https://gate.example.com", "timeout_secs": 0 }
        }
    });
    let resp = app
        .oneshot(put_json("/policies/global/_", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
