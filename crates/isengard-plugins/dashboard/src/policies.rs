//! REST endpoints for update policies.
//!
//! See spec §"REST API (9c)" of
//! `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`.
//!
//! Mounted under `/api/v1` by `lib.rs`. Routes:
//!
//! | Method | Path                                       | Purpose                                |
//! |--------|--------------------------------------------|----------------------------------------|
//! | GET    | `/policies`                                | List all rows in scope-rank order.     |
//! | POST   | `/policies`                                | Insert a new row. 409 on duplicate.    |
//! | PUT    | `/policies/{scope_type}/{*scope_key}`      | Upsert by (scope_type, scope_key).     |
//! | DELETE | `/policies/{scope_type}/{*scope_key}`      | Delete. 404 if absent.                 |
//! | GET    | `/policies/effective?fleet=&stack=&...`    | Resolved policy + provenance.          |
//!
//! `scope_key` is captured as a wildcard so `stack`/`service` keys (which
//! contain `/`) round-trip without URL-encoding gymnastics. For the `global`
//! scope the operator passes a single `_` placeholder which we treat as the
//! empty key (axum's `{*foo}` capture rejects an empty segment).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use chrono::{DateTime, Utc};
use isengard_controller::ControllerHandles;
use isengard_core::policy::{
    Policy, PolicyContext, PolicyScopeType, ResolvedPolicy, parse_cron, resolve_policy,
};
use isengard_storage::policy::{InsertPolicy, PolicyRow};
use serde::{Deserialize, Serialize};

/// Sentinel used in URL paths where the storage `scope_key` is the empty
/// string (i.e. for the `global` scope). axum's `{*scope_key}` capture
/// rejects an empty trailing segment, so we accept `_` and translate.
const GLOBAL_SCOPE_KEY_SENTINEL: &str = "_";

/// Builds the axum router for this resource.
pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/policies", get(list_policies).post(create_policy))
        .route("/policies/effective", get(get_effective_policy))
        .route(
            "/policies/{scope_type}/{*scope_key}",
            put(put_policy).delete(delete_policy),
        )
        .with_state(handles)
}

/// JSON DTO for a `PolicyRow`. Mirrors storage's row but uses camelCase
/// JSON keys for dashboard parity, RFC3339 strings for timestamps, and the
/// already-camelCase serde shape from `Policy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDto {
    /// `id` field.
    pub id: i64,
    /// `scope_type` field.
    pub scope_type: PolicyScopeType,
    /// `scope_key` field.
    pub scope_key: String,
    /// `body` field.
    pub body: Policy,
    /// `created_at` field.
    pub created_at: DateTime<Utc>,
    /// `updated_at` field.
    pub updated_at: DateTime<Utc>,
}

impl From<PolicyRow> for PolicyDto {
    fn from(r: PolicyRow) -> Self {
        Self {
            id: r.id,
            scope_type: r.scope_type,
            scope_key: r.scope_key,
            body: r.body,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Body for `POST /policies`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertPolicyDto {
    /// `scope_type` field.
    pub scope_type: PolicyScopeType,
    /// `scope_key` field.
    pub scope_key: String,
    /// `body` field.
    pub body: Policy,
}

/// Body for `PUT /policies/{scope_type}/{*scope_key}`. Path drives the key,
/// so the body only carries the policy itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutPolicyBodyDto {
    /// `body` field.
    pub body: Policy,
}

/// JSON shape for `GET /policies/effective`. Mirrors `ResolvedPolicy`,
/// kept as a DTO so future field additions stay an explicit choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePolicyDto(pub ResolvedPolicy);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// EffectiveQueryDto.
pub struct EffectiveQueryDto {
    /// `fleet` field.
    pub fleet: Option<String>,
    /// `stack` field.
    pub stack: Option<String>,
    /// `service` field.
    pub service: Option<String>,
    /// `host_id` field.
    pub host_id: Option<String>,
    /// `container` field.
    pub container: Option<String>,
}

/// Standard JSON error envelope used by the policies endpoints.
#[derive(Debug, Serialize)]
struct ErrorBody {
    /// `error` field.
    error: String,
}

/// `err`.
fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

/// Validate `(scope_type, scope_key, body)` per the spec.
///
/// Returns `Ok(())` on success or an HTTP-ready `Response` on failure. The
/// rules are:
///
/// 1. `global` requires an empty `scope_key`; every other scope requires a
///    non-empty `scope_key`.
/// 2. When `body.window` is set, the cron expression must parse.
///    Timezone parsing is intentionally lenient (warn-only on read; we
///    don't reject custom values here) so operators can paste niche IANA
///    names without an API roundtrip.
fn validate_policy(
    scope_type: PolicyScopeType,
    scope_key: &str,
    body: &Policy,
) -> Result<(), Response> {
    match scope_type {
        PolicyScopeType::Global if !scope_key.is_empty() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "scope_key must be empty for scope_type=global",
            ));
        }
        PolicyScopeType::Global => {}
        _ if scope_key.is_empty() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "scope_key must be non-empty for non-global scope_type",
            ));
        }
        _ => {}
    }

    if let Some(window) = &body.window {
        if let Err(e) = parse_cron(&window.cron_expr) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("invalid window cron: {e}"),
            ));
        }
    }

    // External_gate URL must be non-empty when configured.
    if let Some(g) = &body.external_gate {
        if g.url.trim().is_empty() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "external_gate.url must be non-empty",
            ));
        }
        if g.timeout_secs == 0 || g.timeout_secs > 300 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "external_gate.timeout_secs must be in 1..=300",
            ));
        }
    }

    Ok(())
}

/// Decode a `scope_key` captured from the URL. Treats the sentinel `_` as
/// the empty string so the caller can address the `global` row through the
/// same path scheme.
fn decode_scope_key(raw: &str) -> String {
    if raw == GLOBAL_SCOPE_KEY_SENTINEL {
        String::new()
    } else {
        raw.to_string()
    }
}

/// `GET` handler for policies.
async fn list_policies(State(handles): State<Arc<ControllerHandles>>) -> Response {
    match handles.inventory.list_policies().await {
        Ok(rows) => {
            let dtos: Vec<PolicyDto> = rows.into_iter().map(PolicyDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list policies: {e}"),
        ),
    }
}

/// `POST` handler for policy.
async fn create_policy(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<InsertPolicyDto>,
) -> Response {
    if let Err(resp) = validate_policy(body.scope_type, &body.scope_key, &body.body) {
        return resp;
    }

    // Reject duplicates with a clean 409 by checking first; the UNIQUE
    // constraint also catches the race, but the explicit pre-check yields a
    // tidier error envelope on the common case.
    match handles
        .inventory
        .get_policy(body.scope_type, &body.scope_key)
        .await
    {
        Ok(Some(_)) => {
            return err(
                StatusCode::CONFLICT,
                format!(
                    "policy ({}, {}) already exists",
                    body.scope_type.as_str(),
                    body.scope_key
                ),
            );
        }
        Ok(None) => {}
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get policy: {e}"),
            );
        }
    }

    let ins = InsertPolicy {
        scope_type: body.scope_type,
        scope_key: body.scope_key,
        body: body.body,
    };
    match handles.inventory.insert_policy(ins).await {
        Ok(row) => (StatusCode::CREATED, Json(PolicyDto::from(row))).into_response(),
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            err(StatusCode::CONFLICT, format!("policy already exists: {e}"))
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert policy: {e}"),
        ),
    }
}

/// `PUT` handler for policy.
async fn put_policy(
    State(handles): State<Arc<ControllerHandles>>,
    Path((scope_type_s, scope_key_raw)): Path<(String, String)>,
    Json(body): Json<PutPolicyBodyDto>,
) -> Response {
    let scope_type = match scope_type_s.parse::<PolicyScopeType>() {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e}")),
    };
    let scope_key = decode_scope_key(&scope_key_raw);
    if let Err(resp) = validate_policy(scope_type, &scope_key, &body.body) {
        return resp;
    }
    match handles
        .inventory
        .upsert_policy(scope_type, &scope_key, &body.body)
        .await
    {
        Ok(row) => Json(PolicyDto::from(row)).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("upsert policy: {e}"),
        ),
    }
}

/// `DELETE` handler for policy.
async fn delete_policy(
    State(handles): State<Arc<ControllerHandles>>,
    Path((scope_type_s, scope_key_raw)): Path<(String, String)>,
) -> Response {
    let scope_type = match scope_type_s.parse::<PolicyScopeType>() {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e}")),
    };
    let scope_key = decode_scope_key(&scope_key_raw);
    match handles
        .inventory
        .delete_policy(scope_type, &scope_key)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(
            StatusCode::NOT_FOUND,
            format!("policy ({}, {}) not found", scope_type.as_str(), scope_key),
        ),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete policy: {e}"),
        ),
    }
}

/// `GET` handler for effective policy.
async fn get_effective_policy(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<EffectiveQueryDto>,
) -> Response {
    let rows = match handles.inventory.list_policies().await {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list policies: {e}"),
            );
        }
    };

    let projected: Vec<(PolicyScopeType, &str, &Policy)> = rows
        .iter()
        .map(|r| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let ctx = PolicyContext {
        fleet: q.fleet.as_deref(),
        stack: q.stack.as_deref(),
        service: q.service.as_deref(),
        host_id_hex: q.host_id.as_deref(),
        container_name: q.container.as_deref(),
    };
    let resolved = resolve_policy(&projected, &ctx);
    Json(resolved).into_response()
}
