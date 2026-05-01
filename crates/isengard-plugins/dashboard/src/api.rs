//! REST API handlers for the dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use isengard_controller::ControllerHandles;
use isengard_storage::HostId;
use serde_json::json;
use tracing::{debug, warn};

use crate::dto::*;

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/hosts", get(list_hosts).post(enroll_host))
        .route("/hosts/{id}", get(get_host).patch(patch_host).delete(delete_host))
        .route("/hosts/{id}/events", get(host_events))
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
                debug!(state, "list_hosts: state filter accepted but not yet applied");
            }
            Json(dtos).into_response()
        }
        Err(e) => {
            warn!(error = %e, "list_hosts failed");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_hosts: {e}"))
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
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("host_events: {e}")),
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
    State(_handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
    Json(req): Json<PatchHostRequest>,
) -> Response {
    // 5d adds real fleet column to hosts; until then, no-op success.
    debug!(host_id = %id, fleet = ?req.fleet, "patch_host: no-op until 5d migration");
    Json(json!({ "ok": true })).into_response()
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
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("delete_host: {e}")),
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
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_events: {e}")),
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
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("list_fleets: {e}")),
    }
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

    #[tokio::test]
    async fn list_hosts_empty_returns_empty_array() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(Request::builder().uri("/hosts").body(Body::empty()).unwrap())
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
            .oneshot(Request::builder().uri("/hosts").body(Body::empty()).unwrap())
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
}
