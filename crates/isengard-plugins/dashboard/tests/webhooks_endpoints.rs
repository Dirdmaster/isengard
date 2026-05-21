//! Integration tests for `/api/v1/webhooks` (T5, #53).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::webhooks::{self, WebhookCreatedDto, WebhookDto};
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
    let app = webhooks::router(handles.clone());
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

fn post_empty(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn create_returns_secret_once_then_get_returns_masked() {
    let (app, _) = setup_app().await;

    let resp = app
        .clone()
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({
                "url": "https://example.com/hook",
                "secret": "my-known-secret-123",
                "eventKinds": "*",
                "enabled": true,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let created: WebhookCreatedDto = serde_json::from_value(body).unwrap();
    assert_eq!(created.secret, "my-known-secret-123");
    assert!(
        created.webhook.secret_masked.starts_with("****"),
        "masked starts with stars"
    );
    assert!(created.webhook.secret_masked.ends_with("-123"));

    let resp = app
        .oneshot(get_req(&format!("/webhooks/{}", created.webhook.id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dto: WebhookDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(dto.url, "https://example.com/hook");
    assert!(dto.secret_masked.ends_with("-123"));
}

#[tokio::test]
async fn create_auto_generates_secret_when_omitted() {
    let (app, _) = setup_app().await;
    let resp = app
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({
                "url": "https://example.com/hook"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let dto: WebhookCreatedDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(
        dto.secret.len(),
        64,
        "auto-generated secret is 64 hex chars"
    );
    assert!(dto.webhook.enabled);
    assert_eq!(dto.webhook.event_kinds, "*");
}

#[tokio::test]
async fn list_then_update_then_delete_round_trips() {
    let (app, _) = setup_app().await;

    // Create one
    let resp = app
        .clone()
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({
                "url": "https://example.com/a",
                "secret": "s",
                "eventKinds": "update.success",
            }),
        ))
        .await
        .unwrap();
    let created: WebhookCreatedDto = serde_json::from_value(body_json(resp).await).unwrap();

    // List
    let resp = app.clone().oneshot(get_req("/webhooks")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<WebhookDto> = serde_json::from_value(body_json(resp).await).unwrap();
    assert_eq!(list.len(), 1);

    // Update
    let resp = app
        .clone()
        .oneshot(put_json(
            &format!("/webhooks/{}", created.webhook.id),
            serde_json::json!({ "enabled": false, "eventKinds": "*" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: WebhookDto = serde_json::from_value(body_json(resp).await).unwrap();
    assert!(!updated.enabled);
    assert_eq!(updated.event_kinds, "*");

    // Delete
    let resp = app
        .clone()
        .oneshot(delete_req(&format!("/webhooks/{}", created.webhook.id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // 404 after delete
    let resp = app
        .oneshot(get_req(&format!("/webhooks/{}", created.webhook.id)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_endpoint_enqueues_synthetic_delivery() {
    let (app, handles) = setup_app().await;

    let resp = app
        .clone()
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({ "url": "https://example.com/hook" }),
        ))
        .await
        .unwrap();
    let created: WebhookCreatedDto = serde_json::from_value(body_json(resp).await).unwrap();

    let resp = app
        .oneshot(post_empty(&format!(
            "/webhooks/{}/test",
            created.webhook.id
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Confirm a delivery row exists for the webhook with kind=webhook.test.
    let rows = handles
        .inventory
        .list_deliveries(created.webhook.id, None, 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_kind, "webhook.test");
}

#[tokio::test]
async fn list_deliveries_filters_by_status() {
    let (app, handles) = setup_app().await;

    let resp = app
        .clone()
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({ "url": "https://example.com/h" }),
        ))
        .await
        .unwrap();
    let created: WebhookCreatedDto = serde_json::from_value(body_json(resp).await).unwrap();

    // Insert two pending deliveries directly via DAO.
    use isengard_storage::webhook::InsertDelivery;
    for k in ["update.success", "update.failed"] {
        handles
            .inventory
            .insert_delivery(InsertDelivery {
                webhook_id: created.webhook.id,
                event_kind: k.into(),
                payload_json: "{}".into(),
            })
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(get_req(&format!(
            "/webhooks/{}/deliveries?status=pending",
            created.webhook.id
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: serde_json::Value = body_json(resp).await;
    assert_eq!(rows.as_array().unwrap().len(), 2);

    let resp = app
        .oneshot(get_req(&format!(
            "/webhooks/{}/deliveries?status=success",
            created.webhook.id
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: serde_json::Value = body_json(resp).await;
    assert_eq!(rows.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_rejects_empty_url() {
    let (app, _) = setup_app().await;
    let resp = app
        .oneshot(post_json("/webhooks", serde_json::json!({ "url": "" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_returns_404_when_missing() {
    let (app, _) = setup_app().await;
    let resp = app
        .oneshot(put_json(
            "/webhooks/9999",
            serde_json::json!({ "enabled": false }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// Cross-source delivery list endpoint tests.

#[tokio::test]
async fn deliveries_by_source_filters_to_lifecycle_only() {
    use isengard_storage::webhook::{InsertGateDelivery, InsertLifecycleDelivery};
    let (app, handles) = setup_app().await;

    // Seed one webhook delivery (12a path).
    let secret_resp = app
        .clone()
        .oneshot(post_json(
            "/webhooks",
            serde_json::json!({"url": "https://example/h", "eventKinds": "*"}),
        ))
        .await
        .unwrap();
    let created: WebhookCreatedDto = serde_json::from_value(body_json(secret_resp).await).unwrap();
    handles
        .inventory
        .insert_delivery(isengard_storage::webhook::InsertDelivery {
            webhook_id: created.webhook.id,
            event_kind: "update.success".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();

    // Seed one lifecycle, one gate.
    handles
        .inventory
        .insert_lifecycle_delivery(InsertLifecycleDelivery {
            url: "https://h.example/post".into(),
            secret: None,
            event_kind: "deployment.completed".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();
    handles
        .inventory
        .insert_gate_delivery(InsertGateDelivery {
            url: "https://gate.example/decide".into(),
            secret: None,
            event_kind: "update.gate".into(),
            payload_json: "{}".into(),
        })
        .await
        .unwrap();

    // Filter by lifecycle.
    let resp = app
        .clone()
        .oneshot(get_req("/webhooks/deliveries?source=lifecycle"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows = body_json(resp).await;
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["source"], "lifecycle");
}

#[tokio::test]
async fn deliveries_by_source_with_unknown_source_returns_400() {
    let (app, _) = setup_app().await;
    let resp = app
        .oneshot(get_req("/webhooks/deliveries?source=invalid"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
