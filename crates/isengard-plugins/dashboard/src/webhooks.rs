//! REST endpoints for outbound webhooks (Phase 12 Plan A, #53).
//!
//! See spec §"REST endpoints" of
//! `docs/superpowers/specs/2026-05-06-phase-12a-outbound-webhooks-design.md`.
//!
//! Mounted under `/api/v1` by `lib.rs`. Routes:
//!
//! | Method | Path                              | Purpose                  |
//! |--------|-----------------------------------|--------------------------|
//! | GET    | `/webhooks`                       | List all webhooks.       |
//! | POST   | `/webhooks`                       | Create. Secret returned plaintext once. |
//! | GET    | `/webhooks/{id}`                  | Get one. Secret masked.  |
//! | PUT    | `/webhooks/{id}`                  | Update.                  |
//! | DELETE | `/webhooks/{id}`                  | Delete (cascades).       |
//! | GET    | `/webhooks/{id}/deliveries`       | List deliveries.         |
//! | POST   | `/webhooks/{id}/test`             | Enqueue synthetic test.  |

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use isengard_controller::ControllerHandles;
use isengard_storage::webhook::{
    DeliveryStatus, InsertDelivery, InsertWebhook, UpdateWebhook, Webhook, WebhookDelivery,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route(
            "/webhooks/{id}",
            get(get_webhook).put(update_webhook).delete(delete_webhook),
        )
        .route("/webhooks/{id}/deliveries", get(list_deliveries))
        .route("/webhooks/{id}/test", post(test_webhook))
        .with_state(handles)
}

/// Public DTO. The `secret` field is the masked form (last 4 chars of the
/// stored secret) for safe display in lists / detail views.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDto {
    pub id: i64,
    pub url: String,
    pub secret_masked: String,
    pub event_kinds: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Webhook> for WebhookDto {
    fn from(w: Webhook) -> Self {
        Self {
            id: w.id,
            url: w.url,
            secret_masked: mask_secret(&w.secret),
            event_kinds: w.event_kinds,
            enabled: w.enabled,
            created_at: w.created_at,
            updated_at: w.updated_at,
        }
    }
}

/// One-time-show DTO returned ONLY from `POST /webhooks` and the auto-generate
/// flow: includes the plaintext secret. Storage retains the same secret
/// indefinitely (this DTO is just the API response envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookCreatedDto {
    #[serde(flatten)]
    pub webhook: WebhookDto,
    /// Plaintext secret. Returned exactly once on create.
    pub secret: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWebhookDto {
    pub url: String,
    /// Operator-supplied secret. If absent or empty, the server generates one.
    #[serde(default)]
    pub secret: Option<String>,
    /// Comma-separated kinds, or `*`. Defaults to `*`.
    #[serde(default)]
    pub event_kinds: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWebhookDto {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub event_kinds: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDeliveryDto {
    pub id: i64,
    pub webhook_id: i64,
    pub event_kind: String,
    pub status: DeliveryStatus,
    pub attempts: i64,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<WebhookDelivery> for WebhookDeliveryDto {
    fn from(d: WebhookDelivery) -> Self {
        Self {
            id: d.id,
            webhook_id: d.webhook_id,
            event_kind: d.event_kind,
            status: d.status,
            attempts: d.attempts,
            last_attempt_at: d.last_attempt_at,
            last_error: d.last_error,
            next_retry_at: d.next_retry_at,
            created_at: d.created_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveriesQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

/// Show the last 4 chars of the stored secret. Empty / very short secrets
/// produce `****`.
pub fn mask_secret(s: &str) -> String {
    if s.len() <= 4 {
        return "****".to_string();
    }
    let tail: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

fn generate_secret() -> String {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

async fn list_webhooks(State(handles): State<Arc<ControllerHandles>>) -> Response {
    match handles.inventory.list_webhooks().await {
        Ok(rows) => {
            let dtos: Vec<WebhookDto> = rows.into_iter().map(WebhookDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_webhooks: {e}"),
        ),
    }
}

async fn create_webhook(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateWebhookDto>,
) -> Response {
    if body.url.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "url must be non-empty");
    }
    let secret = body
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(generate_secret);
    let event_kinds = body
        .event_kinds
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "*".to_string());

    let ins = InsertWebhook {
        url: body.url,
        secret: secret.clone(),
        event_kinds,
        enabled: body.enabled,
    };
    match handles.inventory.insert_webhook(ins).await {
        Ok(row) => {
            let dto = WebhookCreatedDto {
                webhook: WebhookDto::from(row),
                secret,
            };
            (StatusCode::CREATED, Json(dto)).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert_webhook: {e}"),
        ),
    }
}

async fn get_webhook(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    match handles.inventory.get_webhook(id).await {
        Ok(Some(row)) => Json(WebhookDto::from(row)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "webhook not found"),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get_webhook: {e}"),
        ),
    }
}

async fn update_webhook(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateWebhookDto>,
) -> Response {
    let upd = UpdateWebhook {
        url: body.url,
        secret: body.secret,
        event_kinds: body.event_kinds,
        enabled: body.enabled,
    };
    match handles.inventory.update_webhook(id, upd).await {
        Ok(Some(row)) => Json(WebhookDto::from(row)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "webhook not found"),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update_webhook: {e}"),
        ),
    }
}

async fn delete_webhook(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    match handles.inventory.delete_webhook(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "webhook not found"),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete_webhook: {e}"),
        ),
    }
}

async fn list_deliveries(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    Query(q): Query<DeliveriesQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let status_filter = match q.status.as_deref() {
        Some(s) if !s.is_empty() => match s.parse::<DeliveryStatus>() {
            Ok(p) => Some(p),
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad status: {e}")),
        },
        _ => None,
    };
    match handles
        .inventory
        .list_deliveries(id, status_filter, limit)
        .await
    {
        Ok(rows) => {
            let dtos: Vec<WebhookDeliveryDto> =
                rows.into_iter().map(WebhookDeliveryDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_deliveries: {e}"),
        ),
    }
}

async fn test_webhook(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    // Confirm the webhook exists; gives a tidier 404 than the FK error.
    match handles.inventory.get_webhook(id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, "webhook not found"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_webhook: {e}"),
            );
        }
    }

    let payload = serde_json::json!({
        "kind": "webhook.test",
        "occurred_at": Utc::now().to_rfc3339(),
        "summary": "synthetic test event from isengard dashboard",
        "metadata": {}
    });
    let payload_json = payload.to_string();

    match handles
        .inventory
        .insert_delivery(InsertDelivery {
            webhook_id: id,
            event_kind: "webhook.test".into(),
            payload_json,
        })
        .await
    {
        Ok(d) => (StatusCode::ACCEPTED, Json(WebhookDeliveryDto::from(d))).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert_delivery: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_long_value() {
        assert_eq!(mask_secret("abcdefghij"), "****ghij");
    }

    #[test]
    fn mask_secret_short_value() {
        assert_eq!(mask_secret(""), "****");
        assert_eq!(mask_secret("ab"), "****");
        assert_eq!(mask_secret("abcd"), "****");
    }

    #[test]
    fn generate_secret_is_64_hex_chars() {
        let s = generate_secret();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
