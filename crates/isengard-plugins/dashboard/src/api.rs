//! REST API handlers for the dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
        .route("/stacks", get(list_stacks))
        .route("/stacks/{id}", get(get_stack))
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service))
        .route("/events", get(list_events))
        .route("/events/{id}", get(get_event))
        .route("/fleets", get(list_fleets))
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
    State(_handles): State<Arc<ControllerHandles>>,
    Json(req): Json<EnrollRequest>,
) -> Response {
    // 5e wires real enrollment with a token store. For 5b, return a placeholder.
    debug!(
        fleet = ?req.fleet,
        hostname = ?req.hostname,
        "enroll_host: placeholder response (5e wires real flow)"
    );
    let token = format!("pending-{}", ulid::Ulid::new());
    let cmd = format!(
        "docker run -d --name isengard-agent -e ISENGARD_TOKEN={token} ghcr.io/dirdmaster/isengard:latest agent --controller=https://CONTROLLER_HOST:9417"
    );
    Json(EnrollResponse {
        enrollment_token: token,
        install_command: cmd,
    })
    .into_response()
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
                )
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
    match handles.inventory.list_hosts().await {
        Ok(rows) => {
            let host_count = rows.len();
            let dtos = vec![FleetDto {
                name: "default".into(),
                host_count,
            }];
            Json(dtos).into_response()
        }
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_fleets: {e}"),
        ),
    }
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
            )
        }
    };

    if let Some(fleet) = q.fleet.as_deref() {
        let hosts = match handles.inventory.list_hosts().await {
            Ok(v) => v,
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("list_hosts: {e}"),
                )
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

async fn get_stack(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
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
            )
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use isengard_controller::bus::EventBus;
    use isengard_storage::{EnrollHost, Inventory, Journal};
    use tower::ServiceExt;

    async fn test_handles() -> Arc<ControllerHandles> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let jrnl = Arc::new(Journal::open_in_memory().await.unwrap());
        let bus = Arc::new(EventBus::new());
        Arc::new(ControllerHandles {
            inventory: inv,
            journal: jrnl,
            bus,
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
}
