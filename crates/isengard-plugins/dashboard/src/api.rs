//! REST API handlers for the dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use isengard_controller::ControllerHandles;
use isengard_core::policy::{PolicyContext, resolve_policy};
use isengard_storage::{HostId, InsertStack, StackId, StackSource};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use crate::deployments::DeploymentDto;
use crate::dto::*;

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/hosts", get(list_hosts).post(enroll_host))
        .route(
            "/hosts/{id}",
            get(get_host).patch(patch_host).delete(delete_host),
        )
        .route("/hosts/{id}/events", get(host_events))
        .route("/hosts/{id}/sparkline", get(get_host_sparkline))
        .route("/hosts/{id}/actions/force-update", post(force_update_host))
        .route("/stacks", get(list_stacks).post(create_stack))
        .route("/stacks/{id}", get(get_stack))
        .route(
            "/stacks/{id}/compose",
            get(get_stack_compose).put(put_stack_compose),
        )
        .route(
            "/stacks/{id}/manifest",
            get(get_stack_manifest).put(put_stack_manifest),
        )
        .route("/stacks/{id}/diff", post(post_stack_diff))
        .route(
            "/stacks/{id}/actions/force-update",
            post(force_update_stack),
        )
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service))
        .route(
            "/services/{stack_id}/{service_name}",
            get(get_service_detail),
        )
        .route("/events", get(list_events))
        .route("/events/{id}", get(get_event))
        .route("/fleets", get(list_fleets).post(create_fleet))
        .route("/fleets/{name}", delete(delete_fleet))
        .route("/settings", get(get_settings).patch(patch_settings))
        .with_state(handles)
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn list_hosts(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<HostsQuery>,
) -> Response {
    match handles.inventory.list_hosts().await {
        Ok(rows) => {
            let mut dtos: Vec<HostDto> = rows.into_iter().map(HostDto::from).collect();
            if let Some(fleet) = q.fleet {
                dtos.retain(|h| h.fleet == fleet);
            }
            // 5d wires real state filter; until then we accept and ignore.
            if let Some(state) = q.state {
                debug!(
                    state,
                    "list_hosts: state filter accepted but not yet applied"
                );
            }
            Json(dtos).into_response()
        }
        Err(e) => {
            warn!(error = %e, "list_hosts failed");
            json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_hosts: {e}"),
            )
        }
    }
}

async fn get_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    match handles.inventory.get_host(host_id).await {
        Ok(Some(h)) => Json(HostDto::from(h)).into_response(),
        Ok(None) => json_err(StatusCode::NOT_FOUND, "host not found"),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_host: {e}")),
    }
}

async fn host_events(
    State(handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match handles.journal.list_recent(limit).await {
        Ok(rows) => {
            let dtos: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("host_events: {e}"),
        ),
    }
}

async fn enroll_host(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<EnrollRequest>,
) -> Response {
    // Fleet is required. The wizard prompts for it; there is no implicit
    // 'default' fleet. Trim and validate before doing anything else.
    let fleet = body
        .fleet
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let Some(fleet) = fleet else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "fleet is required (name a fleet to group your hosts)",
        );
    };

    let token = ulid::Ulid::new().to_string();
    let agent_id = ulid::Ulid::new().to_string();

    // Create the fleet now so it shows up in /api/v1/fleets listings even
    // before the agent first enrolls. Idempotent.
    if let Err(e) = handles.inventory.create_fleet(&fleet).await {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create_fleet: {e}"),
        );
    }

    // Persist token in settings (v1.x: dedicated enrollment_tokens table with TTL).
    let payload = serde_json::json!({
        "agent_id": agent_id,
        "fleet": fleet,
        "hostname": body.hostname.clone(),
    });
    if let Err(e) = handles
        .inventory
        .set_setting(&format!("enrollment.token.{token}"), &payload)
        .await
    {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set_setting: {e}"),
        );
    }

    // Resolve the controller URL the agent should dial. Reads from settings
    // (set on first run via Settings → Networking) and falls back to the
    // dashboard's own host. Strips any trailing /api or /install.sh suffix.
    let dashboard_url = match handles.inventory.get_setting("controller.public_url").await {
        Ok(Some(v)) => v.as_str().map(String::from).unwrap_or_else(default_url),
        _ => default_url(),
    };
    let agent_url = dashboard_url
        .replace(":9418", ":9417")
        .trim_end_matches('/')
        .to_string();

    let install_command = render_docker_run_command(&agent_url, &token);

    Json(EnrollmentDto {
        agent_id,
        enrollment_token: token,
        install_command,
    })
    .into_response()
}

fn default_url() -> String {
    "http://controller.local:9418".to_string()
}

/// Builds the multi-line `docker run` command shown to the user during
/// onboarding. Single source of truth: both the wizard and any docs that
/// need to embed an example install command should call this.
fn render_docker_run_command(controller_url: &str, token: &str) -> String {
    [
        "docker run -d --name isengard-agent --restart=always \\".to_string(),
        "  -v /var/run/docker.sock:/var/run/docker.sock \\".to_string(),
        "  -v isengard-agent-data:/var/lib/isengard \\".to_string(),
        format!("  -e CONTROLLER_URL={controller_url} \\"),
        format!("  -e ENROLLMENT_TOKEN={token} \\"),
        "  --group-add $(stat -c %g /var/run/docker.sock) \\".to_string(),
        "  ghcr.io/dirdmaster/isengard-agent:latest".to_string(),
    ]
    .join("\n")
}

async fn force_update_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    match handles
        .inventory
        .queue_action(
            host_id,
            isengard_storage::HostActionKind::ForceUpdate { stack_name: None },
        )
        .await
    {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("queue_action: {e}"),
        ),
    }
}

async fn force_update_stack(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}")),
    };
    match handles
        .inventory
        .queue_action(
            stack.host_id,
            isengard_storage::HostActionKind::ForceUpdate {
                stack_name: Some(stack.name),
            },
        )
        .await
    {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("queue_action: {e}"),
        ),
    }
}

async fn patch_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
    Json(body): Json<PatchHostRequest>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };

    if let Some(fleet) = body.fleet {
        match handles.inventory.set_host_fleet(host_id, &fleet).await {
            Ok(true) => {}
            Ok(false) => return json_err(StatusCode::NOT_FOUND, "host not found"),
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("set_host_fleet: {e}"),
                );
            }
        }
    }

    match handles.inventory.get_host(host_id).await {
        Ok(Some(h)) => Json(HostDto::from(h)).into_response(),
        Ok(None) => json_err(StatusCode::NOT_FOUND, "host not found"),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_host: {e}")),
    }
}

async fn delete_host(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    match handles.inventory.delete_host(host_id).await {
        Ok(deleted) => Json(DeleteResponse { deleted }).into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete_host: {e}"),
        ),
    }
}

async fn list_events(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    // When a `deployment_id` filter is set, widen the journal scan so we
    // don't lose events behind newer unrelated rows. Cap at 5000 rows for
    // safety. Phase 10c (T4 refs #50).
    let scan_limit = if q.deployment_id.is_some() {
        limit.clamp(500, 5000)
    } else {
        limit
    };
    match handles.journal.list_recent(scan_limit).await {
        Ok(rows) => {
            let mut dtos: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
            if let Some(kind) = q.kind {
                dtos.retain(|e| e.kind == kind);
            }
            if let Some(host_id_s) = q.host_id {
                dtos.retain(|e| e.host_id.as_deref() == Some(&host_id_s));
            }
            if let Some(dep_id) = q.deployment_id.as_deref() {
                dtos.retain(|e| {
                    e.metadata
                        .get("deployment")
                        .and_then(|d| d.get("id"))
                        .and_then(|v| v.as_str())
                        == Some(dep_id)
                });
                if dtos.len() as i64 > limit {
                    dtos.truncate(limit as usize);
                }
            }
            Json(dtos).into_response()
        }
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_events: {e}"),
        ),
    }
}

async fn get_event(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id_str): Path<String>,
) -> Response {
    let id: i64 = match id_str.parse() {
        Ok(i) => i,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid event id"),
    };
    match handles.journal.list_recent(500).await {
        Ok(rows) => match rows.into_iter().find(|r| r.id == id) {
            Some(r) => Json(EventDto::from(r)).into_response(),
            None => json_err(StatusCode::NOT_FOUND, "event not found"),
        },
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_event: {e}")),
    }
}

async fn list_fleets(State(handles): State<Arc<ControllerHandles>>) -> Response {
    let fleets = match handles.inventory.list_fleets().await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_fleets: {e}"),
            );
        }
    };
    let hosts = match handles.inventory.list_hosts().await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_hosts: {e}"),
            );
        }
    };

    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for h in hosts {
        *counts.entry(h.fleet).or_default() += 1;
    }

    let dtos: Vec<FleetDto> = fleets
        .into_iter()
        .map(|f| FleetDto {
            name: f.name.clone(),
            host_count: counts.get(&f.name).copied().unwrap_or(0),
            created_at: f.created_at,
        })
        .collect();

    Json(dtos).into_response()
}

async fn create_fleet(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateFleetBody>,
) -> Response {
    if body.name.is_empty() || body.name.len() > 32 {
        return json_err(StatusCode::BAD_REQUEST, "fleet name must be 1-32 chars");
    }
    match handles.inventory.create_fleet(&body.name).await {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create_fleet: {e}"),
        ),
    }
}

async fn delete_fleet(
    State(handles): State<Arc<ControllerHandles>>,
    Path(name): Path<String>,
) -> Response {
    if name == "default" {
        return json_err(StatusCode::BAD_REQUEST, "cannot delete the default fleet");
    }
    match handles.inventory.delete_fleet(&name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "fleet not found"),
        Err(e) => {
            // Distinguish Conflict (has hosts) -> 409
            if matches!(&e, isengard_storage::Error::Conflict(_)) {
                json_err(StatusCode::CONFLICT, e.to_string())
            } else {
                json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("delete_fleet: {e}"),
                )
            }
        }
    }
}

async fn get_settings(State(handles): State<Arc<ControllerHandles>>) -> Response {
    let all = match handles.inventory.list_settings().await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_settings: {e}"),
            );
        }
    };
    let mut values = serde_json::Map::new();
    for s in all {
        if s.key.starts_with("enrollment.token.") {
            continue;
        }
        values.insert(s.key, s.value);
    }
    Json(SettingsDto { values }).into_response()
}

async fn patch_settings(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<PatchSettingsBody>,
) -> Response {
    for (key, value) in body.values {
        if key.starts_with("enrollment.token.") {
            return json_err(
                StatusCode::BAD_REQUEST,
                "cannot set enrollment tokens via settings",
            );
        }
        if let Err(e) = handles.inventory.set_setting(&key, &value).await {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("set_setting: {e}"),
            );
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
struct ListStacksQuery {
    fleet: Option<String>,
    host_id: Option<String>,
}

async fn list_stacks(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListStacksQuery>,
) -> Response {
    let host_filter = match q.host_id.as_deref() {
        Some(s) => match parse_host_id(s) {
            Ok(h) => Some(h),
            Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
        },
        None => None,
    };

    let mut stacks = match handles.inventory.list_stacks(host_filter).await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_stacks: {e}"),
            );
        }
    };

    if let Some(fleet) = q.fleet.as_deref() {
        let hosts = match handles.inventory.list_hosts().await {
            Ok(v) => v,
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("list_hosts: {e}"),
                );
            }
        };
        let allowed: std::collections::HashSet<_> = hosts
            .into_iter()
            .filter(|h| h.fleet == fleet)
            .map(|h| h.id)
            .collect();
        stacks.retain(|s| allowed.contains(&s.host_id));
    }

    let dtos: Vec<StackDto> = stacks.into_iter().map(StackDto::from).collect();
    Json(dtos).into_response()
}

async fn get_stack(State(handles): State<Arc<ControllerHandles>>, Path(id): Path<i64>) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}"));
        }
    };
    // Phase 0.13: include the manifest bundle inline. Legacy compose-only
    // stacks get null / empty fields back; the dashboard JS ignores them.
    let bundle = handles
        .inventory
        .get_stack_manifest_bundle(StackId(id))
        .await
        .unwrap_or_else(|_| isengard_storage::StackManifestBundle {
            manifest_toml: None,
            manifest_sha256: None,
            manifest_imported_at: None,
            deploy_strategy: None,
            manifest_fleet: None,
            secrets: Vec::new(),
            hooks: Vec::new(),
        });
    let mut json = serde_json::to_value(StackDto::from(stack)).unwrap_or_default();
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "manifest_toml".into(),
            serde_json::to_value(&bundle.manifest_toml).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "manifest_sha256".into(),
            serde_json::to_value(&bundle.manifest_sha256).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "manifest_imported_at".into(),
            serde_json::to_value(&bundle.manifest_imported_at).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "deploy_strategy".into(),
            serde_json::to_value(&bundle.deploy_strategy).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "manifest_fleet".into(),
            serde_json::to_value(&bundle.manifest_fleet).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "secrets".into(),
            serde_json::to_value(&bundle.secrets).unwrap_or(serde_json::Value::Null),
        );
        obj.insert(
            "hooks".into(),
            serde_json::to_value(&bundle.hooks).unwrap_or(serde_json::Value::Null),
        );
    }
    Json(json).into_response()
}

/// `GET /api/v1/stacks/:id/compose` (v0.3c).
///
/// Returns the most recent `compose.yaml` the agent reverse-engineered
/// from the running containers, plus its sha256 and import timestamp so
/// the dashboard can show "imported at X" without a separate call.
///
/// Status codes:
/// - `200`: stack exists and the agent has reported a compose.yaml.
/// - `404`: stack id doesn't exist.
/// - `204`: stack exists but no import has been recorded yet (the agent
///   hasn't run a sweep on this host since v0.3c shipped, or the stack
///   isn't `isengard.enable=true`).
async fn get_stack_compose(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}")),
    };
    match handles.inventory.get_stack_compose(stack.id).await {
        Ok(Some(row)) => Json(serde_json::json!({
            "stack_id": stack.id,
            "stack_name": stack.name,
            "compose_yaml": row.yaml,
            "sha256": row.sha256,
            "imported_at": row.imported_at,
        }))
        .into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get_stack_compose: {e}"),
        ),
    }
}

/// `PUT /api/v1/stacks/:id/compose` (v0.3d + Phase 0.13 wave 2.A follow-up).
///
/// Two body shapes, selected by `Content-Type`:
///
/// - `application/yaml` or `text/yaml`: raw YAML body (legacy v0.3d shape).
///   The compose is written; manifest / secrets / hooks state is left
///   unchanged. `If-Match: <sha256>` provides optimistic concurrency.
///
/// - `application/json` (Phase 0.13 follow-up to wave 2.A): structured
///   body that mirrors `POST /api/v1/stacks`:
///   ```json
///   {
///     "compose": "<raw yaml>",
///     "manifest_toml": "<optional stack.toml body>",
///     "secrets": ["..."],
///     "hooks": [{"on": "...", "cmd": [...], "timeout_ms": 60000, "on_error": "abort"}],
///     "force": false,
///     "compose_sha256": "<optional, for optimistic concurrency>",
///     "manifest_sha256": "<optional, advisory>"
///   }
///   ```
///   This is the path operators use to push manifest-only updates to an
///   existing stack (compose unchanged, but secret bindings or hooks
///   change). Before this variant, manifest changes on existing stacks
///   were a no-op via PUT (compose-only) and required rerouting through
///   `POST /stacks`, which is the wrong verb and creates ID churn.
///
/// Optional query: `?force=true` to skip the conflict check. When the
/// JSON body carries `force: true`, that wins too. `false` by default.
///
/// Status codes:
/// - 200: file written; body echoes `{ written_sha256 }`.
/// - 400: hook validation failure, manifest parse error, or invalid JSON.
/// - 409: hash mismatch; body has `current_sha256` + `current_yaml`.
/// - 415: missing or unsupported Content-Type.
/// - 422: unknown secret name (JSON variant).
/// - 503: agent for the stack's host is not currently connected.
/// - 504: agent connected but didn't reply within the timeout.
async fn put_stack_compose(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(q): Query<PutComposeQuery>,
    body: axum::body::Bytes,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}"));
        }
    };

    // Negotiate body shape on Content-Type. Strip any `; charset=...`
    // suffix and lowercase the type so callers can send the canonical
    // forms verbatim. Unknown / missing -> 415 with a hint listing
    // accepted types.
    let ctype = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
        .unwrap_or_default();

    let input = match ctype.as_str() {
        "application/yaml" | "text/yaml" => {
            // Legacy v0.3d shape: raw YAML body. If-Match header carries
            // the expected sha256; compose-only PUT leaves manifest
            // state unchanged on the agent.
            let yaml = match std::str::from_utf8(&body) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    return json_err(StatusCode::BAD_REQUEST, "compose body is not valid UTF-8");
                }
            };
            let expected = headers
                .get("if-match")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_matches('"').to_string())
                .unwrap_or_default();
            PutComposeInput {
                compose_yaml: yaml,
                expected_sha256: expected,
                force: q.force.unwrap_or(false),
                manifest_toml: None,
                secrets: None,
                hooks: None,
            }
        }
        "application/json" => {
            let parsed: PutComposeJsonBody = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return json_err(StatusCode::BAD_REQUEST, format!("invalid json body: {e}"));
                }
            };
            if parsed.compose.trim().is_empty() {
                return json_err(StatusCode::BAD_REQUEST, "compose is empty");
            }
            // `If-Match` header still wins when present; the body's
            // `compose_sha256` is the JSON-native fallback. Keeps HTTP
            // idioms working for JSON callers that don't set headers
            // (e.g. browser fetch() with a minimal init).
            let expected = headers
                .get("if-match")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_matches('"').to_string())
                .or(parsed.compose_sha256.clone())
                .unwrap_or_default();
            // Query `?force=true` OR body `"force": true` wins.
            let force = q.force.unwrap_or(false) || parsed.force.unwrap_or(false);
            PutComposeInput {
                compose_yaml: parsed.compose,
                expected_sha256: expected,
                force,
                manifest_toml: parsed.manifest_toml,
                secrets: parsed.secrets,
                hooks: parsed.hooks,
            }
        }
        "" => {
            return json_err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "missing Content-Type; accepted: application/yaml, application/json",
            );
        }
        other => {
            return json_err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!(
                    "unsupported Content-Type {other:?}; accepted: application/yaml, application/json",
                ),
            );
        }
    };

    // JSON variant: validate + persist manifest bundle BEFORE shipping
    // WriteCompose. If anything bounces (400 / 422), the operator gets
    // a clean error and the agent is never asked to write.
    let manifest_for_agent = match phase_0_13_persist_manifest_bundle(
        &handles,
        stack.id,
        &stack.name,
        input.manifest_toml.as_deref(),
        input.secrets.as_deref(),
        input.hooks.as_deref(),
    )
    .await
    {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let proto_hooks: Vec<isengard_proto::pb::LifecycleHook> = input
        .hooks
        .as_deref()
        .map(|hs| {
            hs.iter()
                .map(|h| isengard_proto::pb::LifecycleHook {
                    on: h.on.clone(),
                    cmd: h.cmd.clone(),
                    timeout_ms: h.timeout_ms,
                    on_error: h.on_error.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let proto_secrets: Vec<String> = input.secrets.clone().unwrap_or_default();

    let request_id = ulid::Ulid::new().to_string();
    let rx = handles.compose_broker.register(request_id.clone()).await;

    let msg = isengard_proto::pb::ControllerMessage {
        payload: Some(
            isengard_proto::pb::controller_message::Payload::WriteCompose(
                isengard_proto::pb::WriteCompose {
                    request_id: request_id.clone(),
                    stack_name: stack.name.clone(),
                    compose_yaml: input.compose_yaml,
                    expected_sha256: input.expected_sha256,
                    force: input.force,
                    manifest_toml: manifest_for_agent,
                    secrets: proto_secrets,
                    hooks: proto_hooks,
                    deployment_id: ulid::Ulid::new().to_string(),
                },
            ),
        ),
    };
    if let Err(e) = handles
        .routing
        .send_message_to_host(stack.host_id, msg)
        .await
    {
        handles.compose_broker.cancel(&request_id).await;
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("agent not connected: {e}"),
        );
    }

    let ack = match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(ack)) => ack,
        Ok(Err(_)) => {
            handles.compose_broker.cancel(&request_id).await;
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "broker dropped sender");
        }
        Err(_) => {
            handles.compose_broker.cancel(&request_id).await;
            return json_err(StatusCode::GATEWAY_TIMEOUT, "agent timed out responding");
        }
    };

    use isengard_proto::pb::write_compose_ack::Kind;
    match Kind::try_from(ack.kind).unwrap_or(Kind::Unspecified) {
        Kind::Ok => Json(serde_json::json!({
            "written_sha256": ack.written_sha256,
        }))
        .into_response(),
        Kind::Conflict => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "compose hash mismatch; reload before saving",
                "current_sha256": ack.current_sha256,
                "current_yaml": ack.current_yaml,
            })),
        )
            .into_response(),
        Kind::Error => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("agent: {}", ack.error),
        ),
        Kind::Unspecified => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent ack kind unspecified",
        ),
    }
}

/// Phase 0.13 (wave 2.A follow-up): JSON body for `PUT /stacks/:id/compose`.
/// Same shape as the manifest fields on `POST /stacks` plus a renamed
/// `compose` (no `_yaml` suffix; the field is the YAML body verbatim).
#[derive(Debug, Deserialize)]
struct PutComposeJsonBody {
    /// Raw compose.yaml content. Required.
    compose: String,
    /// Verbatim stack.toml. Empty / missing leaves the manifest unchanged.
    #[serde(default)]
    manifest_toml: Option<String>,
    /// Per-fleet secret names to bind to this stack. Unknown names yield 422.
    #[serde(default)]
    secrets: Option<Vec<String>>,
    /// Lifecycle hooks. Same shape as `POST /stacks`.
    #[serde(default)]
    hooks: Option<Vec<HookBody>>,
    /// Skip the optimistic concurrency check. Body-level alternative to
    /// the `?force=true` query string. They OR together.
    #[serde(default)]
    force: Option<bool>,
    /// JSON-native alternative to the `If-Match` header. Header wins
    /// when both are present.
    #[serde(default)]
    compose_sha256: Option<String>,
    /// Advisory only: lets callers assert what manifest body they
    /// believe they're overwriting. Not enforced today (manifest
    /// concurrency is server-side last-write-wins); accepted for
    /// forward compatibility with a future manifest-level check.
    #[serde(default, rename = "manifest_sha256")]
    _manifest_sha256: Option<String>,
}

/// Internal shape carrying either the YAML-body or JSON-body PUT input
/// through to the WriteCompose dispatch. Keeps the dispatch single-pass.
struct PutComposeInput {
    compose_yaml: String,
    expected_sha256: String,
    force: bool,
    manifest_toml: Option<String>,
    secrets: Option<Vec<String>>,
    hooks: Option<Vec<HookBody>>,
}

#[derive(Debug, Deserialize)]
struct PutComposeQuery {
    /// Skip the optimistic concurrency check. False / absent by default.
    force: Option<bool>,
}

/// Phase 0.13 follow-up: hook shape in `GET /stacks/{id}/manifest` responses.
/// Mirrors the request-body shape on POST /stacks (`HookBody`) so the client
/// can round-trip a manifest cleanly. `on` and `on_event` track the same
/// field; we expose `on` here to match the manifest TOML schema operators see.
#[derive(Debug, Serialize)]
struct ManifestHookDto {
    on: String,
    cmd: Vec<String>,
    timeout_ms: i64,
    on_error: String,
}

impl From<isengard_storage::StackHook> for ManifestHookDto {
    fn from(h: isengard_storage::StackHook) -> Self {
        Self {
            on: h.on_event,
            cmd: h.cmd,
            timeout_ms: h.timeout_ms,
            on_error: h.on_error,
        }
    }
}

/// `GET /api/v1/stacks/:id/manifest` (Phase 0.13 follow-up).
///
/// Returns the persisted `stack.toml` body for `stack_id`, plus the
/// secrets + hooks bound at deploy time. The operator-side `isd manifest
/// cat / export / edit` chain consumes this surface; the dashboard's
/// "view manifest" affordance will share it.
///
/// Status codes:
/// - 200: stack has a manifest; body has the full bundle.
/// - 204: stack exists but no manifest was ever deployed (legacy
///   compose-only stacks). Empty body.
/// - 404: stack id doesn't exist.
async fn get_stack_manifest(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}"));
        }
    };
    let bundle = match handles.inventory.get_stack_manifest_bundle(stack.id).await {
        Ok(b) => b,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_stack_manifest_bundle: {e}"),
            );
        }
    };
    // Legacy compose-only stacks return 204 so the operator gets a clear
    // signal ("nothing to edit") instead of an empty-string manifest that
    // looks deployable.
    let toml = match bundle.manifest_toml {
        Some(s) if !s.is_empty() => s,
        _ => return StatusCode::NO_CONTENT.into_response(),
    };
    let hooks: Vec<ManifestHookDto> = bundle.hooks.into_iter().map(ManifestHookDto::from).collect();
    Json(serde_json::json!({
        "stack_id": stack.id,
        "stack_name": stack.name,
        "manifest_toml": toml,
        "manifest_sha256": bundle.manifest_sha256,
        "manifest_imported_at": bundle.manifest_imported_at,
        "deploy_strategy": bundle.deploy_strategy,
        "manifest_fleet": bundle.manifest_fleet,
        "secrets": bundle.secrets,
        "hooks": hooks,
    }))
    .into_response()
}

/// Body for `PUT /api/v1/stacks/:id/manifest`.
///
/// `manifest_toml` is required (empty body is rejected with 400 so a
/// fat-fingered editor save doesn't wipe the persisted manifest).
/// `secrets` and `hooks` are optional; when present they replace the
/// persisted set. When absent, the existing bindings stay untouched.
#[derive(Debug, Deserialize)]
struct PutManifestBody {
    manifest_toml: String,
    #[serde(default)]
    secrets: Option<Vec<String>>,
    #[serde(default)]
    hooks: Option<Vec<HookBody>>,
}

/// `PUT /api/v1/stacks/:id/manifest` (Phase 0.13 follow-up).
///
/// Replaces the persisted manifest body (and optionally secrets + hooks)
/// for `stack_id`. Optimistic concurrency: the `If-Match` header carries
/// the sha256 the caller saw on GET. Mismatch yields 409 with the
/// current sha + body so the caller can show a diff and ask the operator
/// to re-edit.
///
/// This endpoint does NOT push compose to the agent: the manifest is the
/// orchestration sidecar (secrets, hooks, fleet, strategy). The on-host
/// compose.yaml is unchanged by this call. A subsequent `isd deploy`
/// re-resolves the merged compose using the new manifest.
///
/// Status codes:
/// - 200: manifest updated; body has `{ manifest_sha256 }`.
/// - 400: manifest parse error, name mismatch, hook validation failure,
///   or empty `manifest_toml`.
/// - 404: stack id doesn't exist.
/// - 409: If-Match mismatch; body has `{ current_sha256, current_toml }`.
/// - 422: unknown secret name(s).
async fn put_stack_manifest(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<PutManifestBody>,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}"));
        }
    };
    if body.manifest_toml.trim().is_empty() {
        return json_err(
            StatusCode::BAD_REQUEST,
            "manifest_toml is empty; PUT requires a non-empty manifest body",
        );
    }

    // Optimistic concurrency. Empty / absent header means "first write".
    let expected = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();
    if !expected.is_empty() {
        let current = match handles.inventory.get_stack_manifest_bundle(stack.id).await {
            Ok(b) => b,
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("get_stack_manifest_bundle: {e}"),
                );
            }
        };
        let current_sha = current.manifest_sha256.unwrap_or_default();
        if current_sha != expected {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "manifest hash mismatch; reload before saving",
                    "current_sha256": current_sha,
                    "current_toml": current.manifest_toml.unwrap_or_default(),
                })),
            )
                .into_response();
        }
    }

    match phase_0_13_persist_manifest_bundle(
        &handles,
        stack.id,
        &stack.name,
        Some(&body.manifest_toml),
        body.secrets.as_deref(),
        body.hooks.as_deref(),
    )
    .await
    {
        Ok(_) => {
            let sha = sha256_hex_of(&body.manifest_toml);
            Json(serde_json::json!({
                "manifest_sha256": sha,
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
struct CreateStackBody {
    /// Stack name. Must be unique per host (the storage layer enforces).
    name: String,
    /// Compose YAML for the new stack. Sent to the agent via WriteCompose
    /// so it lands at `/etc/isengard/stacks/<name>/compose.yml` on the
    /// host. Same wire path as `PUT /stacks/:id/compose`.
    compose_yaml: String,
    /// Target host. Optional: when omitted and exactly one host is
    /// enrolled in the fleet (the homelab single-host case), it's
    /// auto-selected. With multiple hosts, the operator must specify.
    #[serde(default)]
    host_id: Option<String>,
    /// Phase 0.13: verbatim `stack.toml` body. When present, the
    /// controller asserts manifest.name == body.name and stores it
    /// alongside the compose.
    #[serde(default)]
    manifest_toml: Option<String>,
    /// Phase 0.13: per-fleet secret names to bind to this stack. Unknown
    /// names yield 422 with the missing list.
    #[serde(default)]
    secrets: Option<Vec<String>>,
    /// Phase 0.13: lifecycle hooks shaped like the manifest.
    #[serde(default)]
    hooks: Option<Vec<HookBody>>,
}

/// Phase 0.13: hook shape on POST /stacks. Mirrors the TOML manifest.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct HookBody {
    on: String,
    cmd: Vec<String>,
    #[serde(default = "default_hook_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_hook_on_error")]
    on_error: String,
}

fn default_hook_timeout_ms() -> u64 {
    60_000
}
fn default_hook_on_error() -> String {
    "abort".into()
}

#[derive(Debug, Serialize)]
struct CreateStackResponse {
    id: String,
    name: String,
    host_id: String,
    written_sha256: String,
}

/// Phase 0.13: validate + persist a manifest bundle for `stack_id`.
/// Returns the verbatim manifest_toml the controller should ship to the
/// agent in the WriteCompose payload (empty when no manifest was sent).
/// Errors return a fully-formed HTTP response (400 / 422 / 500) the
/// caller can return verbatim.
async fn phase_0_13_persist_manifest_bundle(
    handles: &Arc<ControllerHandles>,
    stack_id: StackId,
    expected_name: &str,
    manifest_toml: Option<&str>,
    secrets: Option<&[String]>,
    hooks: Option<&[HookBody]>,
) -> std::result::Result<String, Response> {
    let manifest_toml_for_agent = manifest_toml.unwrap_or("").to_string();

    if let Some(toml_body) = manifest_toml.filter(|s| !s.is_empty()) {
        // Parse defensively to surface a 400 with the parse error verbatim.
        let parsed = match isengard_manifest::StackManifest::from_str(
            toml_body,
            std::path::PathBuf::from("/"),
        ) {
            Ok(p) => p,
            Err(e) => {
                return Err(json_err(
                    StatusCode::BAD_REQUEST,
                    format!("manifest parse error: {e}"),
                ));
            }
        };
        if parsed.name != expected_name {
            return Err(json_err(
                StatusCode::BAD_REQUEST,
                format!(
                    "manifest name {:?} does not match body name {:?}",
                    parsed.name, expected_name
                ),
            ));
        }
        let sha = sha256_hex_of(toml_body);
        let strategy_str = if matches!(parsed.strategy, isengard_manifest::Strategy::Auto) {
            None
        } else {
            Some(parsed.strategy.as_str())
        };
        // Wave 5.B: auto-create the manifest's fleet if it doesn't already
        // exist. Before this, only the enroll path created fleets, so a
        // stack.toml declaring `fleet = "local"` was silently captured as
        // a dangling reference (no row in `fleets`); `isd ps --fleet local`
        // returned empty and operators had to POST /fleets by hand.
        // `create_fleet` uses INSERT OR IGNORE, so this is idempotent and
        // a no-op when the fleet already exists. The fleet is a logical
        // grouping; nothing in the schema prevents an empty fleet.
        if let Some(fleet_name) = parsed.fleet.as_deref()
            && !fleet_name.trim().is_empty()
            && let Err(e) = handles.inventory.create_fleet(fleet_name).await
        {
            return Err(json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create_fleet: {e}"),
            ));
        }
        if let Err(e) = handles
            .inventory
            .update_stack_manifest(
                stack_id,
                toml_body,
                &sha,
                strategy_str,
                parsed.fleet.as_deref(),
            )
            .await
        {
            return Err(json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("update_stack_manifest: {e}"),
            ));
        }
    }

    if let Some(names) = secrets
        && !names.is_empty()
    {
        let names_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        match handles
            .inventory
            .set_stack_secrets(stack_id, &names_refs)
            .await
        {
            Ok(()) => {}
            Err(isengard_storage::Error::UnknownSecrets(missing)) => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "unknown secrets",
                        "missing": missing,
                    })),
                )
                    .into_response());
            }
            Err(e) => {
                return Err(json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("set_stack_secrets: {e}"),
                ));
            }
        }
    }

    if let Some(hs) = hooks
        && !hs.is_empty()
    {
        for h in hs {
            if !matches!(h.on.as_str(), "pre-deploy" | "post-deploy" | "failure") {
                return Err(json_err(
                    StatusCode::BAD_REQUEST,
                    format!("hook on={:?} is not a valid hook event", h.on),
                ));
            }
            if !matches!(h.on_error.as_str(), "abort" | "continue") {
                return Err(json_err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "hook on_error={:?} must be `abort` or `continue`",
                        h.on_error
                    ),
                ));
            }
            if h.cmd.is_empty() {
                return Err(json_err(
                    StatusCode::BAD_REQUEST,
                    "hook cmd cannot be empty",
                ));
            }
        }
        let stored: Vec<isengard_storage::StackHook> = hs
            .iter()
            .map(|h| isengard_storage::StackHook {
                on_event: h.on.clone(),
                cmd: h.cmd.clone(),
                timeout_ms: h.timeout_ms as i64,
                on_error: h.on_error.clone(),
            })
            .collect();
        if let Err(e) = handles.inventory.set_stack_hooks(stack_id, &stored).await {
            return Err(json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("set_stack_hooks: {e}"),
            ));
        }
    }

    Ok(manifest_toml_for_agent)
}

fn sha256_hex_of(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut out = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `POST /api/v1/stacks`.
///
/// Create a new stack from a compose.yaml. Behaves like `PUT
/// /stacks/:id/compose` but for the case where the stack doesn't exist
/// yet: inserts the row, then forwards the YAML to the agent via the
/// existing WriteCompose path. The CLI uses this for `isd deploy <path>`
/// when the stack name isn't yet in the controller's inventory; the
/// dashboard will use it for the future "new stack" button.
///
/// Phase 0.13: optional `manifest_toml`, `secrets`, `hooks` body fields
/// persist orchestration metadata at the same time. Unknown secret
/// names yield 422 with `{ missing: [...] }`; manifest-name mismatch
/// yields 400.
///
/// Status codes:
/// - 201: stack created; body has `{ id, name, host_id, written_sha256 }`.
/// - 400: bad host_id (not a ULID, no enrolled hosts, or ambiguous);
///   manifest parse error; hook validation failure.
/// - 409: stack name already exists on this host.
/// - 422: one or more secrets in `secrets` are unknown.
/// - 503: agent for the chosen host isn't connected.
/// - 504: agent didn't ack the WriteCompose within the timeout.
async fn create_stack(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateStackBody>,
) -> Response {
    if body.name.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "stack name is empty");
    }
    if body.compose_yaml.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "compose_yaml is empty");
    }

    // Resolve the target host. Operator-supplied host_id wins; otherwise
    // we auto-pick when exactly one host is enrolled (homelab single-host
    // pattern). Multi-host without explicit host_id is rejected with a
    // helpful error so the operator can rerun with --host-id.
    let host_id = match body.host_id.as_deref() {
        Some(s) => match parse_host_id(s) {
            Ok(h) => h,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
        },
        None => {
            let hosts = match handles.inventory.list_hosts().await {
                Ok(h) => h,
                Err(e) => {
                    return json_err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("list_hosts: {e}"),
                    );
                }
            };
            match hosts.len() {
                0 => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "no hosts enrolled; enroll an agent before creating a stack",
                    );
                }
                1 => hosts[0].id,
                _ => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "multiple hosts enrolled; specify `host_id` in the body",
                    );
                }
            }
        }
    };

    let stack_id = match handles
        .inventory
        .insert_stack(InsertStack {
            host_id,
            name: body.name.clone(),
            source: StackSource::Compose,
        })
        .await
    {
        Ok(id) => id,
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            return json_err(
                StatusCode::CONFLICT,
                format!("stack {:?} already exists on this host", body.name),
            );
        }
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("insert_stack: {e}"),
            );
        }
    };

    // Phase 0.13: persist manifest body + secrets + hooks BEFORE the
    // WriteCompose goes out. This keeps the controller's view consistent
    // with what we're about to ship: if the manifest persist fails the
    // operator gets the error and no WriteCompose is dispatched.
    let manifest_toml_for_agent = match phase_0_13_persist_manifest_bundle(
        &handles,
        stack_id,
        &body.name,
        body.manifest_toml.as_deref(),
        body.secrets.as_deref(),
        body.hooks.as_deref(),
    )
    .await
    {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Same WriteCompose path as `put_stack_compose`. force=true because
    // there's nothing on disk to optimistic-conflict against on a fresh
    // stack; expected_sha256="" for the same reason.
    let request_id = ulid::Ulid::new().to_string();
    let rx = handles.compose_broker.register(request_id.clone()).await;

    let proto_hooks = body
        .hooks
        .as_deref()
        .map(|hs| {
            hs.iter()
                .map(|h| isengard_proto::pb::LifecycleHook {
                    on: h.on.clone(),
                    cmd: h.cmd.clone(),
                    timeout_ms: h.timeout_ms,
                    on_error: h.on_error.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let deployment_id = ulid::Ulid::new().to_string();
    let msg = isengard_proto::pb::ControllerMessage {
        payload: Some(
            isengard_proto::pb::controller_message::Payload::WriteCompose(
                isengard_proto::pb::WriteCompose {
                    request_id: request_id.clone(),
                    stack_name: body.name.clone(),
                    compose_yaml: body.compose_yaml,
                    expected_sha256: String::new(),
                    force: true,
                    manifest_toml: manifest_toml_for_agent,
                    secrets: body.secrets.clone().unwrap_or_default(),
                    hooks: proto_hooks,
                    deployment_id,
                },
            ),
        ),
    };
    if let Err(e) = handles.routing.send_message_to_host(host_id, msg).await {
        handles.compose_broker.cancel(&request_id).await;
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("agent not connected: {e}"),
        );
    }

    let ack = match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(ack)) => ack,
        Ok(Err(_)) => {
            handles.compose_broker.cancel(&request_id).await;
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "broker dropped sender");
        }
        Err(_) => {
            handles.compose_broker.cancel(&request_id).await;
            return json_err(StatusCode::GATEWAY_TIMEOUT, "agent timed out responding");
        }
    };

    use isengard_proto::pb::write_compose_ack::Kind;
    match Kind::try_from(ack.kind).unwrap_or(Kind::Unspecified) {
        Kind::Ok => (
            StatusCode::CREATED,
            Json(CreateStackResponse {
                id: stack_id.0.to_string(),
                name: body.name,
                host_id: host_id.to_string(),
                written_sha256: ack.written_sha256,
            }),
        )
            .into_response(),
        // The fresh-stack path uses force=true so a hash conflict here
        // would be surprising; surface it as a 500 with the agent's
        // current state so the operator can debug.
        Kind::Conflict => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "agent reported a hash conflict on a brand-new stack (current_sha256={}); this should be impossible — open an issue",
                ack.current_sha256
            ),
        ),
        Kind::Error => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("agent error writing compose: {}", ack.error),
        ),
        Kind::Unspecified => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent ack kind unspecified",
        ),
    }
}

/// `POST /api/v1/stacks/:id/diff` (v0.3d).
///
/// Body: raw YAML (the operator's proposed compose).
/// Returns the [`isengard_agent::compose_reconciler::ReconcilePlan`] in
/// JSON, or 422 if the YAML doesn't parse. Used by:
/// - the dashboard's "Apply preview" button.
/// - `isd diff <stack>` and `isd apply <path>`.
///
/// The plan is computed against the LAST IMPORTED compose snapshot the
/// controller has cached. The agent runs the actual reconcile against
/// the live container set on apply; small drift between this preview
/// and reality is expected (e.g. operator restarted a container by
/// hand).
async fn post_stack_diff(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
    body: String,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}"));
        }
    };
    let current = match handles.inventory.get_stack_compose(stack.id).await {
        Ok(Some(row)) => row.yaml,
        Ok(None) => String::new(),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_stack_compose: {e}"),
            );
        }
    };
    match crate::compose_diff::diff_yamls(&stack.name, &current, &body) {
        Ok(plan) => Json(plan).into_response(),
        Err(e) => json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("parse compose: {e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ListServicesQuery {
    stack_id: Option<i64>,
    host_id: Option<String>,
    fleet: Option<String>,
}

async fn list_services(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListServicesQuery>,
) -> Response {
    let host_filter = match q.host_id.as_deref() {
        Some(s) => match parse_host_id(s) {
            Ok(h) => Some(h),
            Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
        },
        None => None,
    };

    let stack_filter = q.stack_id.map(StackId);

    let mut services = match handles.inventory.list_services(stack_filter).await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_services: {e}"),
            );
        }
    };

    if let Some(host_id) = host_filter {
        services.retain(|s| s.host_id == host_id);
    }

    // Resolve hostnames once for both fleet filtering and DTO enrichment.
    let hosts = match handles.inventory.list_hosts().await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_hosts: {e}"),
            );
        }
    };

    if let Some(fleet) = q.fleet.as_deref() {
        let allowed: std::collections::HashSet<_> = hosts
            .iter()
            .filter(|h| h.fleet == fleet)
            .map(|h| h.id)
            .collect();
        services.retain(|s| allowed.contains(&s.host_id));
    }

    let hostname_by_id: std::collections::HashMap<_, _> =
        hosts.into_iter().map(|h| (h.id, h.hostname)).collect();

    let dtos: Vec<ServiceDto> = services
        .into_iter()
        .map(|s| {
            let hostname = hostname_by_id.get(&s.host_id).cloned();
            ServiceDto::from_service(s, hostname)
        })
        .collect();
    Json(dtos).into_response()
}

async fn get_service(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
) -> Response {
    json_err(StatusCode::NOT_FOUND, "service not found")
}

/// `GET /api/v1/services/:stack_id/:service_name` (Phase 13A).
///
/// Returns the everything-in-one envelope the service detail page renders:
/// the primary `Service` row for `(stack.host_id, stack_id, service_name)`,
/// any other instances of the same service in the same logical stack on
/// other hosts, the resolved effective policy for the service, the most
/// recent deployment for the service, the last 50 events scoped to the
/// host (filtered to this container when possible), and the routing rules
/// attached to this exact service instance.
async fn get_service_detail(
    State(handles): State<Arc<ControllerHandles>>,
    Path((stack_id, service_name)): Path<(i64, String)>,
) -> Response {
    let stack = match handles.inventory.get_stack(StackId(stack_id)).await {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}")),
    };

    let service = match handles
        .inventory
        .get_service_by_name(stack.host_id, Some(stack.id), &service_name)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "service not found"),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_service_by_name: {e}"),
            );
        }
    };

    // Resolve hostname for the primary service via inventory lookup. Used
    // for display in the metadata card. We allow `None` if the host row is
    // missing (deleted out-of-band): the page still renders.
    let primary_host = match handles.inventory.get_host(service.host_id).await {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_host: {e}")),
    };
    let fleet = primary_host.as_ref().map(|h| h.fleet.clone());
    let primary_hostname = primary_host.as_ref().map(|h| h.hostname.clone());

    // Other instances: services with the same name belonging to a stack
    // with the same `name` on a different host. Walk every host's stacks
    // once, filter, then load the matching service row.
    let other_instances = match collect_other_instances(&handles, &stack, &service).await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("collect_other_instances: {e}"),
            );
        }
    };

    // Effective policy for this scope.
    let policy_rows = match handles.inventory.list_policies().await {
        Ok(r) => r,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_policies: {e}"),
            );
        }
    };
    let projected: Vec<_> = policy_rows
        .iter()
        .map(|r| (r.scope_type, r.scope_key.as_str(), &r.body))
        .collect();
    let host_id_hex = ulid::Ulid::from(service.host_id).to_string();
    let policy_ctx = PolicyContext {
        fleet: fleet.as_deref(),
        stack: Some(stack.name.as_str()),
        service: Some(service.name.as_str()),
        host_id_hex: Some(host_id_hex.as_str()),
        container_name: None,
    };
    let effective_policy = resolve_policy(&projected, &policy_ctx);

    // Last deployment for this service: pull the recent stack deployments
    // and pick the first row that matches the service name.
    let last_deployment = match handles
        .inventory
        .list_deployments_by_stack(stack.id, 50)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .find(|d| d.service_name == service.name)
            .map(DeploymentDto::from),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_deployments_by_stack: {e}"),
            );
        }
    };

    // Recent events: last 50 across the journal, filtered to this host
    // and (when set) the matching container_name.
    let recent_events = match handles.journal.list_recent(500).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| r.host_id == Some(service.host_id))
            .filter(|r| match &r.container_name {
                Some(name) => name == &service.name,
                None => true,
            })
            .take(50)
            .map(EventDto::from)
            .collect::<Vec<_>>(),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_recent: {e}"),
            );
        }
    };

    // Routing rules attached to this exact (host, service_name).
    let routing_rules = match handles
        .inventory
        .list_routing_rules_for_host(service.host_id)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| r.service_name == service.name)
            .collect::<Vec<_>>(),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_routing_rules_for_host: {e}"),
            );
        }
    };

    let dto = ServiceDetailDto {
        service: ServiceDto::from_service(service, primary_hostname),
        other_instances,
        effective_policy,
        last_deployment,
        recent_events,
        routing_rules,
    };
    Json(dto).into_response()
}

/// Find services with the same `name` belonging to stacks named the same
/// as `primary_stack` on hosts other than the primary service's host.
async fn collect_other_instances(
    handles: &Arc<ControllerHandles>,
    primary_stack: &isengard_storage::Stack,
    primary_service: &isengard_storage::Service,
) -> Result<Vec<ServiceDto>, isengard_storage::Error> {
    let stacks = handles.inventory.list_stacks(None).await?;
    let hosts = handles.inventory.list_hosts().await?;
    let host_by_id: std::collections::HashMap<_, _> =
        hosts.iter().map(|h| (h.id, h.hostname.clone())).collect();

    let mut out = Vec::new();
    for s in stacks {
        if s.host_id == primary_service.host_id {
            continue;
        }
        if s.name != primary_stack.name {
            continue;
        }
        let svc = match handles
            .inventory
            .get_service_by_name(s.host_id, Some(s.id), &primary_service.name)
            .await?
        {
            Some(svc) => svc,
            None => continue,
        };
        let hostname = host_by_id.get(&svc.host_id).cloned();
        out.push(ServiceDto::from_service(svc, hostname));
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct SparklineQuery {
    #[serde(default = "default_range")]
    range: String,
}

fn default_range() -> String {
    "24h".to_string()
}

async fn get_host_sparkline(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
    Query(q): Query<SparklineQuery>,
) -> Response {
    let host_id = match parse_host_id(&id) {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
    };
    if q.range != "24h" {
        return json_err(StatusCode::BAD_REQUEST, "only range=24h is supported in v1");
    }

    let now = chrono::Utc::now();
    let since = now - chrono::Duration::hours(24);

    // TODO 5e: add Journal::list_events_for_host(host_id, since) for SQL-level filter
    let rows = match handles.journal.list_recent(500).await {
        Ok(rows) => rows,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_recent: {e}"),
            );
        }
    };

    let mut buckets = vec![0u32; 24];
    for ev in &rows {
        if ev.host_id != Some(host_id) {
            continue;
        }
        if ev.occurred_at < since {
            continue;
        }
        let delta_secs = (now - ev.occurred_at).num_seconds();
        let hours_ago = delta_secs.clamp(0, 23 * 3600) / 3600;
        let idx = (23 - hours_ago) as usize;
        buckets[idx] = buckets[idx].saturating_add(1);
    }
    let total: u32 = buckets.iter().sum();

    Json(SparklineDto {
        buckets,
        range: q.range,
        total,
    })
    .into_response()
}

fn parse_host_id(s: &str) -> Result<HostId, String> {
    let ulid = ulid::Ulid::from_string(s).map_err(|e| format!("invalid host id: {e}"))?;
    Ok(HostId::from(ulid))
}

#[derive(Debug, Deserialize)]
pub struct InstallShQuery {
    pub token: Option<String>,
}

const INSTALL_SH_TEMPLATE: &str = include_str!("../templates/install.sh.tmpl");

pub async fn install_sh(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<InstallShQuery>,
) -> Response {
    let token = match q.token {
        Some(t) => t,
        None => return json_err(StatusCode::BAD_REQUEST, "missing token query"),
    };

    // Basic format guard before hitting the DB (avoid SQL on garbage).
    if token.len() != 26 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return json_err(StatusCode::FORBIDDEN, "invalid token format");
    }

    let key = format!("enrollment.token.{token}");
    let entry = match handles.inventory.get_setting(&key).await {
        Ok(Some(v)) => v,
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "token not found or already used"),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_setting: {e}"),
            );
        }
    };

    let fleet = entry
        .get("fleet")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let hostname = entry.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
    // TODO 5e+: derive controller_url from runtime config rather than hardcoding.
    let controller_url = "http://localhost:9418";

    let body = INSTALL_SH_TEMPLATE
        .replace("{{controller_url}}", controller_url)
        .replace("{{token}}", &token)
        .replace("{{fleet}}", fleet)
        .replace("{{hostname_or_default}}", hostname);

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/x-shellscript; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use isengard_controller::bus::EventBus;
    use isengard_controller::ca::Authority;
    use isengard_controller::enrollment::EnrollmentService;
    use isengard_controller::revocation::RevocationSet;
    use isengard_storage::{EnrollHost, Inventory, Journal};
    use tower::ServiceExt;

    async fn test_handles() -> Arc<ControllerHandles> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let jrnl = Arc::new(Journal::open_in_memory().await.unwrap());
        let bus = Arc::new(EventBus::new());
        let routing = Arc::new(isengard_controller::routing::RoutingPusher::new(
            inv.clone(),
        ));
        let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
        let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
        let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
        Arc::new(ControllerHandles {
            inventory: inv.clone(),
            journal: jrnl,
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
        })
    }

    fn test_enroll() -> EnrollHost {
        EnrollHost {
            fingerprint: "fp-test".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            fleet: "test".into(),
        }
    }

    #[tokio::test]
    async fn list_hosts_empty_returns_empty_array() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hosts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_hosts_returns_enrolled() {
        let handles = test_handles().await;
        handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-test".into(),
                hostname: "h1".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
                fleet: "test".into(),
            })
            .await
            .unwrap();
        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hosts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["hostname"], "h1");
    }

    #[tokio::test]
    async fn list_events_returns_empty_for_fresh_journal() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn delete_host_removes() {
        let handles = test_handles().await;
        let id = handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-del".into(),
                hostname: "h2".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
                fleet: "test".into(),
            })
            .await
            .unwrap();
        let app = router(handles.clone());
        let id_str = ulid::Ulid::from(id).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/hosts/{id_str}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(handles.inventory.get_host(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_stacks_returns_inserted_stacks() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "wordpress".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/stacks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "wordpress");
    }

    #[tokio::test]
    async fn patch_host_updates_fleet() {
        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();

        let app = router(handles.clone());
        let id_str = ulid::Ulid::from(host_id).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/hosts/{id_str}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"fleet": "prod"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let host = handles.inventory.get_host(host_id).await.unwrap().unwrap();
        assert_eq!(host.fleet, "prod");
    }

    #[tokio::test]
    async fn force_update_host_queues_action() {
        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();

        let app = router(handles.clone());
        let id_str = ulid::Ulid::from(host_id).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/hosts/{id_str}/actions/force-update"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let pending = handles.inventory.pending_actions(host_id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].kind,
            isengard_storage::HostActionKind::ForceUpdate { stack_name: None }
        ));
    }

    #[tokio::test]
    async fn create_fleet_then_delete_succeeds() {
        let handles = test_handles().await;
        let app = router(handles.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/fleets")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"prod"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/fleets/prod")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn enroll_host_returns_install_command() {
        let handles = test_handles().await;
        let app = router(handles);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hosts")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"fleet":"staging"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let install_cmd = parsed["install_command"].as_str().unwrap();
        let token = parsed["enrollment_token"].as_str().unwrap();
        assert!(install_cmd.starts_with("docker run"));
        assert!(install_cmd.contains("isengard-agent"));
        assert!(install_cmd.contains("-v /var/run/docker.sock"));
        assert!(install_cmd.contains("CONTROLLER_URL="));
        assert!(install_cmd.contains(&format!("ENROLLMENT_TOKEN={token}")));
    }

    #[tokio::test]
    async fn install_sh_with_valid_token_returns_bash_script() {
        let handles = test_handles().await;
        handles
            .inventory
            .set_setting(
                "enrollment.token.01ARZ3NDEKTSV4RRFFQ69G5FAV",
                &serde_json::json!({
                    "agent_id": "01HX0000000000000000000001",
                    "fleet": "staging",
                    "hostname": null,
                }),
            )
            .await
            .unwrap();

        use axum::Router;
        let app: Router = Router::new()
            .route("/install.sh", get(install_sh))
            .with_state(handles);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/install.sh?token=01ARZ3NDEKTSV4RRFFQ69G5FAV")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/x-shellscript; charset=utf-8"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.starts_with("#!/usr/bin/env bash"));
        assert!(body_str.contains("ISENGARD_TOKEN=\"01ARZ3NDEKTSV4RRFFQ69G5FAV\""));
        assert!(body_str.contains("ISENGARD_FLEET=\"staging\""));
    }

    #[tokio::test]
    async fn install_sh_with_missing_token_returns_400() {
        let handles = test_handles().await;
        use axum::Router;
        let app: Router = Router::new()
            .route("/install.sh", get(install_sh))
            .with_state(handles);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/install.sh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn install_sh_with_unknown_token_returns_403() {
        let handles = test_handles().await;
        use axum::Router;
        let app: Router = Router::new()
            .route("/install.sh", get(install_sh))
            .with_state(handles);

        // 26-char alphanumeric format passes the basic guard but isn't in the DB.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/install.sh?token=01ARZ3NDEKTSV4RRFFQ69G5FAW")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn list_services_returns_persisted_rows_filtered_by_stack() {
        use isengard_storage::{InsertService, InsertStack, ServiceState, StackSource};

        let handles = test_handles().await;

        // Two hosts on different fleets. Each gets a stack with one service.
        let host_a = handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-a".into(),
                hostname: "host-a".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
                fleet: "prod".into(),
            })
            .await
            .unwrap();
        let host_b = handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-b".into(),
                hostname: "host-b".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "v0.1.0".into(),
                docker_version: "27".into(),
                fleet: "staging".into(),
            })
            .await
            .unwrap();

        let stack_a = handles
            .inventory
            .insert_stack(InsertStack {
                host_id: host_a,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let stack_b = handles
            .inventory
            .insert_stack(InsertStack {
                host_id: host_b,
                name: "metrics".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        handles
            .inventory
            .insert_service(InsertService {
                host_id: host_a,
                stack_id: Some(stack_a),
                name: "web".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        handles
            .inventory
            .insert_service(InsertService {
                host_id: host_b,
                stack_id: Some(stack_b),
                name: "prom".into(),
                image: "prom/prometheus:latest".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();

        let app = router(handles.clone());

        // No filter: both services come back, each with its hostname.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/services")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 2, "expected both services with no filter");
        let names: std::collections::HashSet<_> =
            parsed.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains("web"));
        assert!(names.contains("prom"));
        let web = parsed.iter().find(|s| s["name"] == "web").expect("web row");
        assert_eq!(web["hostname"], "host-a");

        // stack_id filter: only stack_a's service.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/services?stack_id={}", stack_a.0))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "web");

        // fleet filter: only services on hosts in the matching fleet.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/services?fleet=staging")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "prom");
        assert_eq!(parsed[0]["hostname"], "host-b");
    }

    /// Wave 5.B: a stack.toml that declares `fleet = "<name>"` for a
    /// fleet the controller doesn't know about must auto-create that
    /// fleet row before the manifest persists. Before this fix, only
    /// the enroll path created fleets, so manifest-only deploys (no
    /// fresh host) left the fleet field dangling.
    ///
    /// We exercise `phase_0_13_persist_manifest_bundle` directly because
    /// the full POST /stacks path also dispatches a WriteCompose RPC to
    /// an agent connection that doesn't exist in the in-memory test
    /// harness; the auto-create runs strictly before that dispatch and
    /// is the only behaviour we care about here.
    #[tokio::test]
    async fn manifest_with_unknown_fleet_auto_creates_fleet_row() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        // Pre-condition: no fleet named "local" exists yet (the enroll
        // above pinned the host to "test").
        let before = handles.inventory.list_fleets().await.unwrap();
        assert!(
            !before.iter().any(|f| f.name == "local"),
            "test harness setup leaked a 'local' fleet"
        );

        let manifest_toml = "name = \"blog\"\nfleet = \"local\"\ncompose = [\"compose.yaml\"]\n";
        phase_0_13_persist_manifest_bundle(
            &handles,
            stack_id,
            "blog",
            Some(manifest_toml),
            None,
            None,
        )
        .await
        .expect("manifest with unknown fleet should auto-create the fleet row");

        // Post-condition: the fleet row exists.
        let after = handles.inventory.list_fleets().await.unwrap();
        assert!(
            after.iter().any(|f| f.name == "local"),
            "expected `local` fleet auto-created by manifest persist; got {:?}",
            after.iter().map(|f| &f.name).collect::<Vec<_>>(),
        );

        // Idempotency: re-running the same persist call must not error
        // and must not duplicate the row.
        phase_0_13_persist_manifest_bundle(
            &handles,
            stack_id,
            "blog",
            Some(manifest_toml),
            None,
            None,
        )
        .await
        .expect("second persist call should be a no-op for the fleet");
        let after_twice = handles.inventory.list_fleets().await.unwrap();
        let count = after_twice.iter().filter(|f| f.name == "local").count();
        assert_eq!(count, 1, "no duplicate fleet rows on repeated persist");
    }

    /// Phase 0.13 wave 2.A follow-up: `PUT /stacks/{id}/compose` with no
    /// Content-Type returns 415 with the accepted types listed. Operators
    /// shouldn't ever land here in practice (curl/reqwest set the header
    /// when given a body), but a tight 415 keeps the failure mode legible.
    #[tokio::test]
    async fn put_compose_missing_content_type_returns_415() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .body(Body::from("services:\n  web:\n    image: nginx\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let err = parsed["error"].as_str().unwrap_or("");
        assert!(err.contains("Content-Type"), "got: {err}");
        assert!(err.contains("application/yaml"), "got: {err}");
        assert!(err.contains("application/json"), "got: {err}");
    }

    /// Phase 0.13 wave 2.A follow-up: unknown content-type (e.g. plain
    /// text) is 415 with the same error shape.
    #[tokio::test]
    async fn put_compose_unknown_content_type_returns_415() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "text/plain")
                    .body(Body::from("services:\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// Phase 0.13 wave 2.A follow-up: YAML content-type (legacy v0.3d
    /// shape) still routes through. Without an agent attached, the
    /// dispatch reaches the routing layer and bounces with 503: that's
    /// the proof the body shape was accepted and the YAML branch ran.
    #[tokio::test]
    async fn put_compose_yaml_content_type_still_works() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/yaml")
                    .body(Body::from("services:\n  web:\n    image: nginx\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // No agent connected in the test harness: the routing layer
        // returns 503. The crucial bit is that the request was accepted
        // (not 415), proving the YAML branch ran. text/yaml + charset
        // suffix variants share this code path.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Phase 0.13 wave 2.A follow-up: JSON variant with manifest body +
    /// secrets + hooks is accepted. Reaches the routing layer (503 here)
    /// only after the manifest bundle has been validated and persisted;
    /// post-condition: the manifest_toml row is set on the stack.
    #[tokio::test]
    async fn put_compose_json_persists_manifest_then_dispatches() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles.clone());
        let manifest = "name = \"blog\"\nfleet = \"test\"\ncompose = [\"compose.yaml\"]\n";
        let body = serde_json::json!({
            "compose": "services:\n  web:\n    image: nginx\n",
            "manifest_toml": manifest,
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Same 503 ceiling as the YAML test (no agent). Below the
        // ceiling we expect the manifest to have been persisted before
        // the routing layer was reached.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let s = handles
            .inventory
            .get_stack(stack_id)
            .await
            .unwrap()
            .expect("stack present");
        assert_eq!(s.manifest_toml.as_deref(), Some(manifest));
    }

    /// Phase 0.13 wave 2.A follow-up: 422 with `missing: [...]` when the
    /// JSON body's `secrets` references a name the controller doesn't
    /// know about. Validation runs before the WriteCompose dispatch so
    /// no agent traffic is generated.
    #[tokio::test]
    async fn put_compose_json_unknown_secret_returns_422() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let body = serde_json::json!({
            "compose": "services:\n  web:\n    image: nginx\n",
            "secrets": ["nonexistent_secret"],
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"], "unknown secrets");
        assert_eq!(parsed["missing"][0], "nonexistent_secret");
    }

    /// Phase 0.13 wave 2.A follow-up: 400 when the JSON body's hook has
    /// an invalid `on_error` value. The validation runs through the
    /// shared `phase_0_13_persist_manifest_bundle` helper.
    #[tokio::test]
    async fn put_compose_json_invalid_hook_returns_400() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let body = serde_json::json!({
            "compose": "services:\n  web:\n    image: nginx\n",
            "hooks": [{
                "on": "pre-deploy",
                "cmd": ["echo", "hi"],
                "timeout_ms": 60000,
                "on_error": "explode",
            }],
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Phase 0.13 wave 2.A follow-up: JSON body with an empty `compose`
    /// field is 400 (the field is required and non-empty).
    #[tokio::test]
    async fn put_compose_json_empty_compose_returns_400() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let body = serde_json::json!({ "compose": "   " }).to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Phase 0.13 wave 2.A follow-up: content-type with a charset suffix
    /// (`application/json; charset=utf-8`) still routes to the JSON
    /// branch. Browsers and many HTTP clients add the suffix by default.
    #[tokio::test]
    async fn put_compose_json_with_charset_suffix_still_dispatches() {
        use isengard_storage::{InsertStack, StackSource};

        let handles = test_handles().await;
        let host_id = handles.inventory.enroll_host(test_enroll()).await.unwrap();
        let stack_id = handles
            .inventory
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();

        let app = router(handles);
        let body = serde_json::json!({
            "compose": "services:\n  web:\n    image: nginx\n",
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/stacks/{}/compose", stack_id.0))
                    .header("content-type", "application/json; charset=utf-8")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Reached routing layer (no agent): proves the JSON branch ran
        // despite the charset suffix.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
