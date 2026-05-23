//! REST endpoints for `isd configure` (v0.7).
//!
//! Endpoints (mounted under `/api/v1`):
//!
//! | Method | Path                  | Purpose                                                       |
//! |--------|-----------------------|---------------------------------------------------------------|
//! | GET    | `/config`             | List every schema key with its current value.                 |
//! | GET    | `/config/schema`      | Echo the static schema verbatim.                              |
//! | GET    | `/config/{key}`       | Read one key.                                                 |
//! | PUT    | `/config/{key}`       | Write one key. Validated against the schema.                  |
//! | DELETE | `/config/{key}`       | Unset one key. 404 when the key was already unset.            |
//! | POST   | `/config/zones/cloudflare-fetch` | Fetch zones from Cloudflare for the operator to confirm. |
//!
//! Secret-typed keys are redacted by default on `GET /config` and
//! `GET /config/{key}`; pass `?show_secret=1` (single-key GET) or
//! `?show_secrets=1` (list) to receive the cleartext value. The PUT
//! handler trusts the schema's type validation: it accepts the inline
//! string for secret-typed keys, with the CLI-side guard refusing
//! inline secret values at the user surface (PR 3).
//!
//! These routes share the dashboard plugin's mTLS gate with every other
//! `/api/v1/*` endpoint; no extra auth wiring lives here. Plaintext
//! secret values flow through this layer briefly on PUT (in the request
//! body) and on GET with `?show_secret=1`; logs never carry the value,
//! only the key.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, response::Json as JsonResp};
use isengard_controller::ControllerHandles;
use isengard_controller::cloudflare;
use isengard_controller::config::{ConfigValue, KeyType, SchemaEntry};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Builds the axum router for the configure routes.
///
/// Order matters: the static `/config/schema` route is registered
/// before the dynamic `/config/{key}` route so that a literal `schema`
/// in the path resolves to `config_schema` rather than getting
/// captured as the `key` parameter.
pub fn router(handles: Arc<ControllerHandles>) -> Router {
    router_with_cf_base(handles, None)
}

/// Same as [`router`] but lets tests override the Cloudflare API base
/// URL so wiremock can stand in for the public CF endpoint. `None`
/// keeps the production base URL hard-coded inside the controller's
/// [`cloudflare`] module.
pub fn router_with_cf_base(handles: Arc<ControllerHandles>, cf_base_url: Option<String>) -> Router {
    Router::new()
        .route("/config", get(list_config))
        .route("/config/schema", get(config_schema))
        .route(
            "/config/zones/cloudflare-fetch",
            post(post_cloudflare_zones),
        )
        .route(
            "/config/{key}",
            get(get_config).put(put_config).delete(delete_config),
        )
        .with_state(ConfigState {
            handles,
            cf_base_url,
        })
}

/// Router state. Bundles the controller handles and the optional CF
/// override so the test harness can swap in a wiremock endpoint without
/// reaching into the controller crate.
#[derive(Clone)]
struct ConfigState {
    /// Controller handles, same as every other dashboard route.
    handles: Arc<ControllerHandles>,
    /// `Some(url)` makes [`post_cloudflare_zones`] route through that
    /// base URL; `None` uses the production CF endpoint.
    cf_base_url: Option<String>,
}

/// PUT body for `/config/{key}`.
#[derive(Debug, Deserialize)]
pub struct PutBody {
    /// New value. JSON type must satisfy the schema entry's
    /// [`KeyType`]; otherwise the controller returns 400.
    pub value: Value,
}

/// Query string for the single-key GET.
#[derive(Debug, Deserialize, Default)]
pub struct GetQuery {
    /// When truthy, secret-typed keys are returned in cleartext.
    /// Accepts `1`, `true`, `yes`, `on` (case-insensitive); anything
    /// else is treated as `false`. Defaults to redacting.
    #[serde(default, deserialize_with = "deser_truthy")]
    pub show_secret: bool,
}

/// Query string for the list GET.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// When truthy, secret-typed keys are returned in cleartext.
    /// Same parsing rules as [`GetQuery::show_secret`]. Defaults to
    /// redacting.
    #[serde(default, deserialize_with = "deser_truthy")]
    pub show_secrets: bool,
}

/// Permissive boolean parser for query strings: `1`, `true`, `yes`,
/// `on` (case-insensitive) all yield `true`; everything else yields
/// `false`. The dashboard sends literal `?show_secret=1` from the CLI,
/// so a strict serde bool would 400 the request.
fn deser_truthy<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

/// JSON shape for one key (single-key GET).
#[derive(Debug, Serialize)]
pub struct GetResponse {
    /// Echoed schema key.
    pub key: String,
    /// Schema-declared type. Renders as a lower-snake-case string.
    #[serde(rename = "type")]
    pub ty: KeyType,
    /// Current value. Either the stored value, the schema default, or
    /// `<redacted>` for secrets without `?show_secret=1`.
    pub value: Value,
    /// `"set"` when the value came from the backing store,
    /// `"default"` when it came from the schema.
    pub source: &'static str,
    /// `true` when a backing-store row exists for `key`.
    pub is_set: bool,
}

/// One row in the list response.
#[derive(Debug, Serialize)]
pub struct ListRow {
    /// Schema key.
    pub key: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    pub ty: KeyType,
    /// Current value: stored, default, redacted, or `null` when both
    /// the backing store and the schema have no value.
    pub value: Value,
    /// `"set"` (stored), `"default"` (schema default), or `"unset"`
    /// (no value).
    pub source: &'static str,
}

/// Build a JSON error response with a stable shape:
/// `{"error": "<message>"}`.
fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, JsonResp(json!({ "error": msg.into() }))).into_response()
}

/// `GET /api/v1/config/{key}` handler.
async fn get_config(
    State(state): State<ConfigState>,
    Path(key): Path<String>,
    Query(q): Query<GetQuery>,
) -> Response {
    let dispatcher = state.handles.config_dispatcher();
    let entry = match dispatcher.schema().get(&key) {
        Some(e) => e,
        None => return err(StatusCode::NOT_FOUND, format!("unknown config key: {key}")),
    };
    match dispatcher.get(&key).await {
        Ok(Some(ConfigValue::Set(v))) => {
            let value = if entry.is_secret() && !q.show_secret {
                Value::String("<redacted>".into())
            } else {
                v
            };
            JsonResp(GetResponse {
                key,
                ty: entry.ty,
                value,
                source: "set",
                is_set: true,
            })
            .into_response()
        }
        Ok(Some(ConfigValue::Default(v))) => JsonResp(GetResponse {
            key,
            ty: entry.ty,
            value: v,
            source: "default",
            is_set: false,
        })
        .into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("config key {key} is unset")),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `PUT /api/v1/config/{key}` handler.
async fn put_config(
    State(state): State<ConfigState>,
    Path(key): Path<String>,
    Json(body): Json<PutBody>,
) -> Response {
    match state
        .handles
        .config_dispatcher()
        .put(&key, body.value, Some("dashboard"))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `DELETE /api/v1/config/{key}` handler.
async fn delete_config(State(state): State<ConfigState>, Path(key): Path<String>) -> Response {
    match state.handles.config_dispatcher().delete(&key).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(
            StatusCode::NOT_FOUND,
            format!("config key {key} is not set"),
        ),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `GET /api/v1/config` handler. Snapshots every schema key.
async fn list_config(State(state): State<ConfigState>, Query(q): Query<ListQuery>) -> Response {
    let rows = match state.handles.config_dispatcher().list().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let out: Vec<ListRow> = rows
        .into_iter()
        .map(|(entry, value)| {
            let (value, source) = match value {
                Some(ConfigValue::Set(v)) => {
                    if entry.is_secret() && !q.show_secrets {
                        (Value::String("<redacted>".into()), "set")
                    } else {
                        (v, "set")
                    }
                }
                Some(ConfigValue::Default(v)) => (v, "default"),
                None => (Value::Null, "unset"),
            };
            ListRow {
                key: entry.key.into(),
                ty: entry.ty,
                value,
                source,
            }
        })
        .collect();
    JsonResp(out).into_response()
}

/// `GET /api/v1/config/schema` handler. Echoes the static schema
/// verbatim so the CLI can drive its help text + did-you-mean off the
/// server's view.
async fn config_schema(State(state): State<ConfigState>) -> Response {
    let entries: Vec<SchemaEntry> = state
        .handles
        .config_dispatcher()
        .schema()
        .entries()
        .to_vec();
    JsonResp(entries).into_response()
}

/// `POST /api/v1/config/zones/cloudflare-fetch` handler.
///
/// Reads the `cloudflare.api_token` secret via the dispatcher; calls
/// [`cloudflare::list_zones`] with it; returns the resulting zone names
/// as `{"zones": [...]}`. Does NOT auto-populate `routing.zones`: the
/// CLI/TUI shows the fetched list to the operator and writes only after
/// confirmation.
///
/// Status codes:
///
/// | Code | Reason                                              |
/// |------|-----------------------------------------------------|
/// | 200  | Fetch succeeded. Body: `{"zones": [...]}`.          |
/// | 400  | `cloudflare.api_token` is unset.                    |
/// | 502  | Upstream Cloudflare error (message bubbled).        |
async fn post_cloudflare_zones(State(state): State<ConfigState>) -> Response {
    let dispatcher = state.handles.config_dispatcher();
    let token = match dispatcher.get("cloudflare.api_token").await {
        Ok(Some(ConfigValue::Set(Value::String(s)))) if !s.is_empty() => s,
        Ok(Some(ConfigValue::Set(_))) | Ok(Some(ConfigValue::Default(_))) | Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                "cloudflare.api_token is not set; set it first",
            );
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let result = match state.cf_base_url.as_deref() {
        Some(base) => cloudflare::list_zones_with_base(&token, base).await,
        None => cloudflare::list_zones(&token).await,
    };
    match result {
        Ok(zones) => JsonResp(json!({ "zones": zones })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_row_serializes_type_as_snake_case() {
        let row = ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set",
        };
        let raw = serde_json::to_string(&row).unwrap();
        // `type` is renamed; the enum variant serializes as snake_case.
        assert!(raw.contains("\"type\":\"secret\""), "raw: {raw}");
        assert!(raw.contains("\"source\":\"set\""), "raw: {raw}");
    }

    #[test]
    fn get_response_serializes_value_field() {
        let r = GetResponse {
            key: "acme.directory".into(),
            ty: KeyType::String,
            value: Value::String("https://example.com".into()),
            source: "default",
            is_set: false,
        };
        let raw = serde_json::to_string(&r).unwrap();
        assert!(raw.contains("\"is_set\":false"), "raw: {raw}");
        assert!(raw.contains("\"source\":\"default\""), "raw: {raw}");
        assert!(raw.contains("\"type\":\"string\""), "raw: {raw}");
    }
}
