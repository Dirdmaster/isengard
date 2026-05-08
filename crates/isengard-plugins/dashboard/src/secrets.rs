//! REST endpoints for the v0.3.6 Isengard-managed secrets store.
//!
//! Endpoints (mounted under `/api/v1`):
//!
//! | Method | Path                  | Purpose                                                   |
//! |--------|-----------------------|-----------------------------------------------------------|
//! | POST   | `/secrets`            | Create a new secret. 409 if `name` exists; PUT to replace.|
//! | PUT    | `/secrets/{name}`     | Upsert: replace if present, insert otherwise.             |
//! | GET    | `/secrets`            | List `(name, created_at, updated_at)`. NEVER values.      |
//! | DELETE | `/secrets/{name}`     | Remove a secret. 204 on success, 404 if missing.          |
//!
//! Auth: these run on the dashboard plugin's HTTP port, which is currently
//! unauthenticated per Phase 14. The operator binds the dashboard behind
//! their own access control (Cloudflare Access, mTLS, VPN). v1.x will add
//! a first-party gate; until then we rely on the existing stance.
//!
//! Plaintext values are accepted on POST/PUT and never logged. Logs refer
//! to secrets by `name` only. The list endpoint NEVER returns values.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, response::Json as JsonResp};
use isengard_controller::ControllerHandles;
use isengard_controller::secrets::SecretsError;
use isengard_storage::SecretMeta;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/secrets", post(create_secret).get(list_secrets))
        .route(
            "/secrets/{name}",
            axum::routing::put(put_secret).delete(delete_secret),
        )
        .with_state(handles)
}

/// POST /api/v1/secrets body shape. `value` is the raw plaintext bytes
/// shipped as a UTF-8 string. Binary values can be base64-encoded by the
/// operator (the controller doesn't care: we treat the whole field as
/// opaque bytes for storage). The full body is held briefly in memory and
/// dropped right after encryption.
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub name: String,
    pub value: String,
}

/// PUT /api/v1/secrets/{name} body shape. Name comes from the URL.
#[derive(Debug, Deserialize)]
pub struct PutBody {
    pub value: String,
}

/// Public-safe secret metadata. Mirrors [`isengard_storage::SecretMeta`]
/// but without `created_by` (operator-only attribution) so the JSON shape
/// is small + stable for the dashboard.
#[derive(Debug, Serialize)]
pub struct SecretListEntry {
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub created_by: Option<String>,
}

impl From<SecretMeta> for SecretListEntry {
    fn from(m: SecretMeta) -> Self {
        Self {
            name: m.name,
            created_at: m.created_at.to_rfc3339(),
            updated_at: m.updated_at.to_rfc3339(),
            created_by: m.created_by,
        }
    }
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, JsonResp(json!({ "error": msg.into() }))).into_response()
}

/// Map a [`SecretsError`] to the right HTTP status. Plaintext is never in
/// the error message; the worst we leak is a name that the caller already
/// knew.
fn map_secrets_err(e: SecretsError) -> Response {
    match e {
        SecretsError::MasterKeyMissing => json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "controller secrets store is locked: master key file not readable",
        ),
        SecretsError::MasterKeyUnreadable(_) => json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "controller secrets store is locked: master key file not readable",
        ),
        SecretsError::MasterKeyWrongSize { actual } => json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("master key file is the wrong size ({actual} bytes; expected 32)"),
        ),
        SecretsError::CiphertextTruncated { actual } => {
            warn!(actual, "secrets ciphertext truncated; DB row corrupted");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "decrypt error")
        }
        SecretsError::NotFound(name) => {
            json_err(StatusCode::NOT_FOUND, format!("secret {name:?} not found"))
        }
        SecretsError::Storage(isengard_storage::Error::Conflict(msg)) => {
            json_err(StatusCode::CONFLICT, msg)
        }
        SecretsError::Storage(e) => {
            // Validation errors come through as Decode; map them to 400.
            let s = format!("{e}");
            if s.contains("invalid") || s.contains("must be") || s.contains("max is") {
                json_err(StatusCode::BAD_REQUEST, s)
            } else {
                warn!(error = %e, "secrets storage error");
                json_err(StatusCode::INTERNAL_SERVER_ERROR, "storage error")
            }
        }
        SecretsError::Encrypt(e) => {
            warn!(error = %e, "secrets encrypt error");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "encrypt error")
        }
        SecretsError::Decrypt(e) => {
            warn!(error = %e, "secrets decrypt error");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "decrypt error")
        }
        SecretsError::Io(e) => {
            warn!(error = %e, "secrets io error");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "io error")
        }
    }
}

async fn create_secret(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateBody>,
) -> Response {
    if body.name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "name is required");
    }
    let bytes = body.value.into_bytes();
    match handles
        .secrets
        .create(&body.name, &bytes, Some("dashboard"))
        .await
    {
        Ok(()) => (StatusCode::CREATED, JsonResp(json!({ "name": body.name }))).into_response(),
        Err(e) => map_secrets_err(e),
    }
}

async fn put_secret(
    State(handles): State<Arc<ControllerHandles>>,
    Path(name): Path<String>,
    Json(body): Json<PutBody>,
) -> Response {
    if name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "name is required");
    }
    let bytes = body.value.into_bytes();
    match handles.secrets.put(&name, &bytes, Some("dashboard")).await {
        Ok(_inserted) => (StatusCode::OK, JsonResp(json!({ "name": name }))).into_response(),
        Err(e) => map_secrets_err(e),
    }
}

async fn list_secrets(State(handles): State<Arc<ControllerHandles>>) -> Response {
    match handles.secrets.list().await {
        Ok(metas) => {
            let dtos: Vec<SecretListEntry> = metas.into_iter().map(SecretListEntry::from).collect();
            JsonResp(dtos).into_response()
        }
        Err(e) => map_secrets_err(e),
    }
}

async fn delete_secret(
    State(handles): State<Arc<ControllerHandles>>,
    Path(name): Path<String>,
) -> Response {
    match handles.secrets.delete(&name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, format!("secret {name:?} not found")),
        Err(e) => map_secrets_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_entry_omits_created_by_when_none() {
        let m = SecretMeta {
            name: "k".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: None,
        };
        let dto = SecretListEntry::from(m);
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"created_by\":null"));
        assert!(!json.contains("ciphertext"));
    }

    #[test]
    fn list_entry_serialization_never_includes_ciphertext_field() {
        let m = SecretMeta {
            name: "k".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: Some("operator".into()),
        };
        let dto: SecretListEntry = m.into();
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("ciphertext"));
        assert!(!json.contains("value"));
    }

    #[tokio::test]
    async fn map_secrets_err_master_key_missing_is_503() {
        let resp = map_secrets_err(SecretsError::MasterKeyMissing);
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn map_secrets_err_master_key_wrong_size_is_503() {
        let resp = map_secrets_err(SecretsError::MasterKeyWrongSize { actual: 16 });
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn map_secrets_err_not_found_is_404() {
        let resp = map_secrets_err(SecretsError::NotFound("x".into()));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn map_secrets_err_conflict_is_409() {
        let resp = map_secrets_err(SecretsError::Storage(isengard_storage::Error::Conflict(
            "secret \"x\" already exists".into(),
        )));
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn map_secrets_err_invalid_name_is_400() {
        // The storage Decode error path: validate_secret_name yields it.
        let resp = map_secrets_err(SecretsError::Storage(isengard_storage::Error::Decode {
            reason: "secret name \"!\" contains invalid chars".into(),
        }));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
