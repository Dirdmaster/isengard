//! Dashboard REST surface for containers.
//!
//! `GET  /api/v1/containers` lists rows from the `containers` table.
//! Query params: `host`, `stack`, `service`, `state`, `all`, `limit`,
//! `offset`. `all=true` includes rows whose `removed_at` is non-NULL.
//!
//! `GET /api/v1/containers/:id` fetches a single row by its 16-char
//! operator id.
//!
//! Host-offline derivation: containers join to hosts; if a container's
//! host has not heartbeated within [`HOST_OFFLINE_THRESHOLD_SECS`]
//! seconds, the response sets `host_offline = true` and
//! `host_offline_secs` to `now - hosts.last_seen_at`. The container's
//! own `state` is left as last reported; the client renders the
//! offline qualifier in its STATUS column.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use isengard_controller::ControllerHandles;
use isengard_storage::containers::{ContainerListFilter, get_container, list_containers};
use isengard_storage::host::HostId;
use serde::Deserialize;
use serde_json::json;

use crate::dto::ContainerDto;

/// A host whose `last_seen_at` is older than this many seconds is
/// flagged as offline in the container list response. Default 60s per
/// spec; matches the v0.18 charter's default-and-document table.
pub const HOST_OFFLINE_THRESHOLD_SECS: i64 = 60;

/// Query params for `GET /api/v1/containers`. Every field is optional;
/// missing fields apply no filter. `all = true` includes rows with a
/// non-NULL `removed_at` (defaults to false).
#[derive(Debug, Deserialize, Default)]
pub struct ListContainersQuery {
    /// `host` field.
    pub host: Option<String>,
    /// `stack` field.
    pub stack: Option<String>,
    /// `service` field.
    pub service: Option<String>,
    /// `state` field.
    pub state: Option<String>,
    #[serde(default)]
    /// `all` field.
    pub all: bool,
    /// `limit` field.
    pub limit: Option<i64>,
    /// `offset` field.
    pub offset: Option<i64>,
}

/// `json_err`.
fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// `parse_host_id`.
fn parse_host_id(s: &str) -> Result<HostId, String> {
    let ulid = ulid::Ulid::from_string(s).map_err(|e| format!("invalid host id: {e}"))?;
    Ok(HostId::from(ulid))
}

/// `GET /api/v1/containers`. Returns the projected DTO list ordered
/// `last_seen_at DESC, id ASC` (delegated to the DAO).
pub async fn list_containers_handler(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListContainersQuery>,
) -> Response {
    let host_filter = match q.host.as_deref() {
        Some(s) => match parse_host_id(s) {
            Ok(h) => Some(h),
            Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
        },
        None => None,
    };

    let filter = ContainerListFilter {
        host_id: host_filter,
        stack: q.stack,
        service: q.service,
        state: q.state,
        include_removed: q.all,
        limit: q.limit,
        offset: q.offset,
    };

    let rows = match list_containers(handles.inventory.pool(), filter).await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_containers: {e}"),
            );
        }
    };

    // One host lookup per request (not per row); cheap on a few-dozen-
    // host fleet and avoids N+1 queries.
    let hosts = match handles.inventory.list_hosts().await {
        Ok(v) => v,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_hosts: {e}"),
            );
        }
    };
    let hostname_by_id: HashMap<HostId, String> =
        hosts.iter().map(|h| (h.id, h.hostname.clone())).collect();
    let last_seen_by_id: HashMap<HostId, Option<i64>> =
        hosts.iter().map(|h| (h.id, h.last_seen_at)).collect();

    let now_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let dtos: Vec<ContainerDto> = rows
        .into_iter()
        .map(|row| {
            let host_name = hostname_by_id.get(&row.host_id).cloned();
            let last_seen = last_seen_by_id.get(&row.host_id).copied().flatten();
            let (host_offline, host_offline_secs) = derive_host_offline(last_seen, now_seconds);
            ContainerDto::from_row(row, host_name, host_offline, host_offline_secs)
        })
        .collect();
    Json(dtos).into_response()
}

/// `GET /api/v1/containers/:id`. Returns the single row or a 404.
pub async fn get_container_handler(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Response {
    let row = match get_container(handles.inventory.pool(), &id).await {
        Ok(Some(r)) => r,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "container not found"),
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get_container: {e}"),
            );
        }
    };

    let host_name = match handles.inventory.get_host(row.host_id).await {
        Ok(Some(h)) => Some(h.hostname),
        _ => None,
    };
    let last_seen = match handles.inventory.get_host(row.host_id).await {
        Ok(Some(h)) => h.last_seen_at,
        _ => None,
    };
    let now_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (host_offline, host_offline_secs) = derive_host_offline(last_seen, now_seconds);
    Json(ContainerDto::from_row(
        row,
        host_name,
        host_offline,
        host_offline_secs,
    ))
    .into_response()
}

/// Pure helper: given the host's `last_seen_at` (unix seconds, None
/// for never-heard) and the current time, return whether the host is
/// past [`HOST_OFFLINE_THRESHOLD_SECS`] and the offline duration. A
/// host that has never heartbeated is considered offline with a 0-sec
/// duration (the row still belongs to a known host; the dashboard
/// shows "(host offline)" without a duration).
fn derive_host_offline(last_seen: Option<i64>, now_seconds: i64) -> (bool, i64) {
    match last_seen {
        Some(ts) => {
            let delta = (now_seconds - ts).max(0);
            (delta > HOST_OFFLINE_THRESHOLD_SECS, delta)
        }
        None => (true, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use isengard_controller::ControllerHandles;
    use isengard_controller::bus::EventBus;
    use isengard_controller::ca::Authority;
    use isengard_controller::enrollment::EnrollmentService;
    use isengard_controller::revocation::RevocationSet;
    use isengard_storage::containers::{ContainerRow, upsert_container};
    use isengard_storage::host::{EnrollHost, HostId};
    use isengard_storage::{Inventory, Journal};
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
            ca,
            ssh_ca: Arc::new(isengard_controller::ssh_ca::SshAuthority::for_tests().unwrap()),
            config_dispatcher: ControllerHandles::test_config_dispatcher(
                inv.clone(),
                Arc::new(isengard_controller::secrets::SecretsStore::new_locked(
                    inv.clone(),
                )),
            ),
        })
    }

    fn router(handles: Arc<ControllerHandles>) -> Router {
        Router::new()
            .route("/containers", get(list_containers_handler))
            .route("/containers/{id}", get(get_container_handler))
            .with_state(handles)
    }

    fn sample_row(host_id: HostId, id: &str, runtime_id: &str) -> ContainerRow {
        ContainerRow {
            id: id.into(),
            host_id,
            service_id: None,
            runtime_container_id: runtime_id.into(),
            image: "nginx:alpine".into(),
            command: Some("nginx -g 'daemon off;'".into()),
            state: "running".into(),
            status_message: Some("Up 5m".into()),
            names: format!("{runtime_id}-name"),
            stack: Some("hello".into()),
            service: Some("web".into()),
            created_at: Some(1_700_000_000),
            first_seen_at: 1_700_000_100,
            last_seen_at: 1_700_000_200,
            removed_at: None,
        }
    }

    async fn enroll(handles: &ControllerHandles, hostname: &str, fp: &str) -> HostId {
        handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: fp.into(),
                hostname: hostname.into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
            })
            .await
            .unwrap()
    }

    /// Helper: hit the router and decode the JSON array body.
    async fn get_list(router: &Router, uri: &str) -> (StatusCode, Vec<serde_json::Value>) {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        if body.is_empty() {
            return (status, Vec::new());
        }
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap_or_default();
        (status, parsed)
    }

    #[tokio::test]
    async fn list_no_filter_returns_alive_rows() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        upsert_container(pool, &sample_row(host, "id-1", "rt-1"))
            .await
            .unwrap();
        upsert_container(pool, &sample_row(host, "id-2", "rt-2"))
            .await
            .unwrap();

        let app = router(handles);
        let (status, rows) = get_list(&app, "/containers").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.len(), 2);
        let ids: Vec<_> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"id-1"));
        assert!(ids.contains(&"id-2"));
    }

    #[tokio::test]
    async fn list_host_filter_isolates_rows() {
        let handles = test_handles().await;
        let host_a = enroll(&handles, "host-a", "fp-a").await;
        let host_b = enroll(&handles, "host-b", "fp-b").await;
        let pool = handles.inventory.pool();
        upsert_container(pool, &sample_row(host_a, "id-a", "rt-a"))
            .await
            .unwrap();
        upsert_container(pool, &sample_row(host_b, "id-b", "rt-b"))
            .await
            .unwrap();

        let app = router(handles);
        let uri = format!("/containers?host={}", host_a);
        let (status, rows) = get_list(&app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "id-a");
    }

    #[tokio::test]
    async fn list_stack_filter_isolates_rows() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        let mut hello = sample_row(host, "id-h", "rt-h");
        hello.stack = Some("hello".into());
        let mut other = sample_row(host, "id-o", "rt-o");
        other.stack = Some("other".into());
        upsert_container(pool, &hello).await.unwrap();
        upsert_container(pool, &other).await.unwrap();

        let app = router(handles);
        let (status, rows) = get_list(&app, "/containers?stack=hello").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["stack"], "hello");
    }

    #[tokio::test]
    async fn list_service_filter_isolates_rows() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        let mut web = sample_row(host, "id-w", "rt-w");
        web.service = Some("web".into());
        let mut db = sample_row(host, "id-d", "rt-d");
        db.service = Some("db".into());
        upsert_container(pool, &web).await.unwrap();
        upsert_container(pool, &db).await.unwrap();

        let app = router(handles);
        let (status, rows) = get_list(&app, "/containers?service=db").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["service"], "db");
    }

    #[tokio::test]
    async fn list_state_filter_isolates_rows() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        let mut running = sample_row(host, "id-r", "rt-r");
        running.state = "running".into();
        let mut exited = sample_row(host, "id-e", "rt-e");
        exited.state = "exited".into();
        upsert_container(pool, &running).await.unwrap();
        upsert_container(pool, &exited).await.unwrap();

        let app = router(handles);
        let (status, rows) = get_list(&app, "/containers?state=exited").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "exited");
    }

    #[tokio::test]
    async fn list_all_true_includes_removed() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        let alive = sample_row(host, "id-alive", "rt-alive");
        let mut removed = sample_row(host, "id-removed", "rt-removed");
        removed.removed_at = Some(1_700_000_500);
        upsert_container(pool, &alive).await.unwrap();
        upsert_container(pool, &removed).await.unwrap();

        let app = router(handles);

        // Default: only alive row.
        let (_, rows) = get_list(&app, "/containers").await;
        assert_eq!(rows.len(), 1);

        // all=true: includes removed.
        let (_, rows) = get_list(&app, "/containers?all=true").await;
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn host_offline_flag_set_when_host_stale() {
        let handles = test_handles().await;
        let stale = enroll(&handles, "stale-host", "fp-stale").await;
        let fresh = enroll(&handles, "fresh-host", "fp-fresh").await;
        // Heartbeat stale host an hour ago, fresh host at `now`.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        handles
            .inventory
            .touch_host(stale, now - 3600)
            .await
            .unwrap();
        handles.inventory.touch_host(fresh, now).await.unwrap();

        let pool = handles.inventory.pool();
        upsert_container(pool, &sample_row(stale, "id-stale", "rt-stale"))
            .await
            .unwrap();
        upsert_container(pool, &sample_row(fresh, "id-fresh", "rt-fresh"))
            .await
            .unwrap();

        let app = router(handles);
        let (status, rows) = get_list(&app, "/containers").await;
        assert_eq!(status, StatusCode::OK);
        let stale_row = rows
            .iter()
            .find(|r| r["id"] == "id-stale")
            .expect("stale row present");
        assert_eq!(stale_row["host_offline"], serde_json::Value::Bool(true));
        assert!(stale_row["host_offline_secs"].as_i64().unwrap() >= 3600);

        let fresh_row = rows
            .iter()
            .find(|r| r["id"] == "id-fresh")
            .expect("fresh row present");
        assert_eq!(fresh_row["host_offline"], serde_json::Value::Bool(false));
    }

    #[tokio::test]
    async fn get_by_id_returns_200_when_present() {
        let handles = test_handles().await;
        let host = enroll(&handles, "h1", "fp-1").await;
        let pool = handles.inventory.pool();
        upsert_container(pool, &sample_row(host, "abc123", "rt-1"))
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/containers/abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["id"], "abc123");
        assert_eq!(parsed["runtime_container_id"], "rt-1");
    }

    #[tokio::test]
    async fn get_by_id_returns_404_when_absent() {
        let handles = test_handles().await;
        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/containers/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
