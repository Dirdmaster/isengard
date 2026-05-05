//! REST API handlers for the dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use isengard_controller::ControllerHandles;
use isengard_storage::{HostId, StackId};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

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
        .route("/stacks", get(list_stacks))
        .route("/stacks/{id}", get(get_stack))
        .route(
            "/stacks/{id}/actions/force-update",
            post(force_update_stack),
        )
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service))
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
/// onboarding. Single source of truth — both the wizard and any docs that
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
    match handles.journal.list_recent(limit).await {
        Ok(rows) => {
            let mut dtos: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
            if let Some(kind) = q.kind {
                dtos.retain(|e| e.kind == kind);
            }
            if let Some(host_id_s) = q.host_id {
                dtos.retain(|e| e.host_id.as_deref() == Some(&host_id_s));
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
    match handles.inventory.get_stack(StackId(id)).await {
        Ok(Some(s)) => Json(StackDto::from(s)).into_response(),
        Ok(None) => json_err(StatusCode::NOT_FOUND, "stack not found"),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("get_stack: {e}")),
    }
}

#[derive(Debug, Deserialize)]
struct ListServicesQuery {
    #[allow(dead_code)] // Reserved for 5e when services are persisted.
    stack_id: Option<i64>,
}

async fn list_services(
    State(_handles): State<Arc<ControllerHandles>>,
    Query(_q): Query<ListServicesQuery>,
) -> Response {
    // v1: services aren't persisted yet (5e adds the services table). Stack
    // info carries service names in the proto, but we don't materialize them
    // server-side for this query. Return empty for now; the UI handles this.
    Json(Vec::<ServiceDto>::new()).into_response()
}

async fn get_service(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<String>,
) -> Response {
    json_err(StatusCode::NOT_FOUND, "service not found")
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
            inventory: inv,
            journal: jrnl,
            bus,
            routing,
            enrollment,
            revocation,
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
}
