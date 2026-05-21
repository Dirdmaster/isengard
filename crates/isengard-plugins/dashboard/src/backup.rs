//! Backup REST endpoints.
//!
//! Routes:
//! - GET  /api/v1/backup/config    -> current config (secrets masked)
//! - PUT  /api/v1/backup/config    -> upsert config
//! - POST /api/v1/backup/run-now   -> trigger an immediate snapshot
//! - GET  /api/v1/backup/runs      -> recent runs history
//!
//! The handlers consult `isengard_plugin_backup::runner_handle()` for the
//! shared `BackupRunner`. If the backup plugin hasn't started yet (e.g.
//! during early controller boot), run-now responds with 503; the other
//! routes still work because they only read settings and run history.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use isengard_controller::ControllerHandles;
use isengard_plugin_backup::config::{BackupConfig, DestinationConfig};
use isengard_plugin_backup::encrypt::passphrase_fingerprint;
use isengard_plugin_backup::runner_handle;
use isengard_storage::{BackupRun, BackupRunStatus, RestoreRun, RestoreRunStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

/// `SECRET_MASK` constant.
const SECRET_MASK: &str = "***";

/// Builds the axum router for this resource.
pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/backup/config", get(get_config).put(put_config))
        .route("/backup/run-now", post(run_now))
        .route("/backup/runs", get(list_runs))
        .route("/backup/runs/{id}/manifest", get(get_run_manifest))
        .route("/backup/restore", post(restore))
        .route("/backup/restore-runs", get(list_restore_runs))
        .with_state(handles)
}

/// `json_err`.
fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

// ---------------- DTOs ----------------

/// What clients see/send for the config. Mirrors `BackupConfig` but masks
/// secrets on GET. PUT accepts either the masked value (treated as "leave
/// unchanged") or a real value (replaces what's stored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfigDto {
    /// `enabled` field.
    pub enabled: bool,
    /// `destination` field.
    pub destination: DestinationConfig,
    /// `interval_secs` field.
    pub interval_secs: u64,
    /// `retention_keep` field.
    pub retention_keep: u32,
    /// `passphrase_fingerprint` field.
    pub passphrase_fingerprint: String,
    /// Sent on PUT to update the fingerprint without ever revealing the
    /// passphrase to the dashboard server. Optional; if present, recomputes
    /// `passphrase_fingerprint`. The raw value is discarded after hashing.
    #[serde(default, skip_serializing)]
    pub passphrase: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
/// BackupRunDto.
pub struct BackupRunDto {
    /// `id` field.
    pub id: i64,
    /// `started_at` field.
    pub started_at: String,
    /// `finished_at` field.
    pub finished_at: Option<String>,
    /// `status` field.
    pub status: String,
    /// `object_name` field.
    pub object_name: Option<String>,
    /// `size_bytes` field.
    pub size_bytes: Option<i64>,
    /// `error` field.
    pub error: Option<String>,
}

impl From<BackupRun> for BackupRunDto {
    fn from(r: BackupRun) -> Self {
        Self {
            id: r.id.0,
            started_at: r.started_at.to_rfc3339(),
            finished_at: r.finished_at.map(|t| t.to_rfc3339()),
            status: match r.status {
                BackupRunStatus::Running => "running".into(),
                BackupRunStatus::Success => "success".into(),
                BackupRunStatus::Failed => "failed".into(),
            },
            object_name: r.object_name,
            size_bytes: r.size_bytes,
            error: r.error,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
/// RestoreRunDto.
pub struct RestoreRunDto {
    /// `id` field.
    pub id: i64,
    /// `source_object` field.
    pub source_object: String,
    /// `source_backup_run_id` field.
    pub source_backup_run_id: Option<i64>,
    /// `started_at` field.
    pub started_at: String,
    /// `finished_at` field.
    pub finished_at: Option<String>,
    /// `status` field.
    pub status: String,
    /// `previous_db_backup_path` field.
    pub previous_db_backup_path: Option<String>,
    /// `bytes_restored` field.
    pub bytes_restored: Option<i64>,
    /// `error` field.
    pub error: Option<String>,
}

impl From<RestoreRun> for RestoreRunDto {
    fn from(r: RestoreRun) -> Self {
        Self {
            id: r.id.0,
            source_object: r.source_object,
            source_backup_run_id: r.source_backup_run_id,
            started_at: r.started_at.to_rfc3339(),
            finished_at: r.finished_at.map(|t| t.to_rfc3339()),
            status: match r.status {
                RestoreRunStatus::Running => "running".into(),
                RestoreRunStatus::Success => "success".into(),
                RestoreRunStatus::Failed => "failed".into(),
            },
            previous_db_backup_path: r.previous_db_backup_path,
            bytes_restored: r.bytes_restored,
            error: r.error,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// RestoreRequestDto.
pub struct RestoreRequestDto {
    /// Object name on the destination, e.g. `snapshot-20260506T120000Z.db.age`.
    pub object_name: String,
    /// `passphrase` field.
    pub passphrase: String,
    #[serde(default)]
    /// `dry_run` field.
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
/// RestoreOutcomeDto.
pub struct RestoreOutcomeDto {
    /// `run_id` field.
    pub run_id: i64,
    /// `source_object` field.
    pub source_object: String,
    /// `restored_at` field.
    pub restored_at: String,
    /// `previous_db_backup_path` field.
    pub previous_db_backup_path: String,
    /// `bytes_restored` field.
    pub bytes_restored: u64,
    /// `dry_run` field.
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
/// BackupRunManifestDto.
pub struct BackupRunManifestDto {
    /// `id` field.
    pub id: i64,
    /// `object_name` field.
    pub object_name: String,
    /// `size_bytes` field.
    pub size_bytes: i64,
    /// `started_at` field.
    pub started_at: String,
    /// `finished_at` field.
    pub finished_at: Option<String>,
    /// Stored fingerprint of the passphrase the backup was encrypted with
    /// (12 hex chars, SHA-256 prefix). The UI compares this against the
    /// fingerprint of the passphrase the operator pasted to confirm a match
    /// before the destructive confirm step.
    pub passphrase_fingerprint: String,
}

// ---------------- Handlers ----------------

/// `mask`.
fn mask(cfg: BackupConfig) -> BackupConfigDto {
    let dest = match cfg.destination {
        DestinationConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
        } => DestinationConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key: if secret_access_key.is_empty() {
                String::new()
            } else {
                SECRET_MASK.into()
            },
        },
        other => other,
    };
    BackupConfigDto {
        enabled: cfg.enabled,
        destination: dest,
        interval_secs: cfg.interval_secs,
        retention_keep: cfg.retention_keep,
        passphrase_fingerprint: cfg.passphrase_fingerprint,
        passphrase: None,
    }
}

/// `GET` handler for config.
async fn get_config(State(handles): State<Arc<ControllerHandles>>) -> Response {
    match BackupConfig::load(&handles.inventory).await {
        Ok(cfg) => Json(mask(cfg)).into_response(),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("load backup config: {e}"),
        ),
    }
}

/// `PUT` handler for config.
async fn put_config(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<BackupConfigDto>,
) -> Response {
    // Reject obviously bad inputs.
    if body.interval_secs == 0 {
        return json_err(StatusCode::BAD_REQUEST, "interval_secs must be > 0");
    }
    if body.retention_keep == 0 {
        return json_err(StatusCode::BAD_REQUEST, "retention_keep must be >= 1");
    }

    // For S3 destinations: if the secret is the mask, fetch the existing
    // value and reuse it so the operator can edit other fields without
    // re-typing the secret.
    let destination = match body.destination {
        DestinationConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
        } => {
            let resolved_secret = if secret_access_key == SECRET_MASK {
                let prev = BackupConfig::load(&handles.inventory).await.ok();
                match prev.map(|p| p.destination) {
                    Some(DestinationConfig::S3 {
                        secret_access_key: s,
                        ..
                    }) => s,
                    _ => String::new(),
                }
            } else {
                secret_access_key
            };
            DestinationConfig::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                secret_access_key: resolved_secret,
            }
        }
        other => other,
    };

    // Recompute the fingerprint if the operator pasted a passphrase. Otherwise
    // keep whatever fingerprint they sent (which the UI sources from a prior
    // GET). We never persist or echo the passphrase itself.
    let fingerprint = match body.passphrase {
        Some(p) if !p.is_empty() => passphrase_fingerprint(&p),
        _ => body.passphrase_fingerprint,
    };

    let cfg = BackupConfig {
        enabled: body.enabled,
        destination,
        interval_secs: body.interval_secs,
        retention_keep: body.retention_keep,
        passphrase_fingerprint: fingerprint,
    };

    if let Err(e) = cfg.save(&handles.inventory).await {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("save backup config: {e}"),
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `run_now`.
async fn run_now(State(_handles): State<Arc<ControllerHandles>>) -> Response {
    let runner = match runner_handle() {
        Some(r) => r,
        None => {
            warn!("run-now requested but backup plugin runner is not yet started");
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "backup plugin runner is not yet started; try again shortly",
            );
        }
    };

    match runner.run_once().await {
        Ok(id) => (StatusCode::ACCEPTED, Json(json!({ "run_id": id.0 }))).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("run_once: {e}")),
    }
}

#[derive(Debug, Deserialize)]
/// RunsQuery.
struct RunsQuery {
    #[serde(default)]
    /// `limit` field.
    limit: Option<u32>,
}

/// `GET` handler for runs.
async fn list_runs(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<RunsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    match handles.inventory.list_backup_runs(limit).await {
        Ok(rows) => {
            let dtos: Vec<BackupRunDto> = rows.into_iter().map(BackupRunDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list backup runs: {e}"),
        ),
    }
}

/// GET /backup/runs/{id}/manifest: pre-flight info for a restore.
///
/// Returns the object name, size, timestamps, and the controller's stored
/// passphrase fingerprint. The UI hashes the operator's pasted passphrase
/// client-side and compares to `passphrase_fingerprint`; if they match, the
/// confirm step proceeds.
async fn get_run_manifest(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<i64>,
) -> Response {
    let runs = match handles.inventory.list_backup_runs(200).await {
        Ok(r) => r,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list backup runs: {e}"),
            );
        }
    };
    let run = match runs.into_iter().find(|r| r.id.0 == id) {
        Some(r) => r,
        None => return json_err(StatusCode::NOT_FOUND, format!("backup run {id} not found")),
    };
    if run.status != BackupRunStatus::Success {
        return json_err(
            StatusCode::CONFLICT,
            format!("backup run {id} is not in success state"),
        );
    }
    let object_name = match run.object_name {
        Some(n) => n,
        None => {
            return json_err(
                StatusCode::CONFLICT,
                format!("backup run {id} has no object name"),
            );
        }
    };

    let cfg = match BackupConfig::load(&handles.inventory).await {
        Ok(c) => c,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load backup config: {e}"),
            );
        }
    };

    let dto = BackupRunManifestDto {
        id: run.id.0,
        object_name,
        size_bytes: run.size_bytes.unwrap_or(0),
        started_at: run.started_at.to_rfc3339(),
        finished_at: run.finished_at.map(|t| t.to_rfc3339()),
        passphrase_fingerprint: cfg.passphrase_fingerprint,
    };
    Json(dto).into_response()
}

/// POST /backup/restore: synchronous restore. Returns the outcome on
/// success (200), or 4xx for user errors (wrong passphrase, missing
/// object, runner not started, missing fields), 5xx for infrastructure
/// failures (network / disk / migrate).
async fn restore(
    State(_handles): State<Arc<ControllerHandles>>,
    Json(body): Json<RestoreRequestDto>,
) -> Response {
    if body.object_name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "object_name is required");
    }
    if body.passphrase.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "passphrase is required");
    }

    let runner = match runner_handle() {
        Some(r) => r,
        None => {
            warn!("restore requested but backup plugin runner is not yet started");
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "backup plugin runner is not yet started; try again shortly",
            );
        }
    };

    match runner
        .restore_now(&body.object_name, &body.passphrase, body.dry_run)
        .await
    {
        Ok(o) => {
            let dto = RestoreOutcomeDto {
                run_id: o.run_id,
                source_object: o.source_object,
                restored_at: o.restored_at.to_rfc3339(),
                previous_db_backup_path: o.previous_db_backup_path,
                bytes_restored: o.bytes_restored,
                dry_run: o.dry_run,
            };
            Json(dto).into_response()
        }
        Err(e) => {
            use isengard_plugin_backup::restore::RestoreError;
            let status = match &e {
                RestoreError::EmptyPassphrase => StatusCode::BAD_REQUEST,
                RestoreError::Decrypt(_) => StatusCode::BAD_REQUEST,
                RestoreError::InvalidSnapshot(_) => StatusCode::BAD_REQUEST,
                RestoreError::Destination(_) => StatusCode::BAD_GATEWAY,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            json_err(status, format!("restore: {e}"))
        }
    }
}

/// `GET` handler for restore runs.
async fn list_restore_runs(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<RunsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    match handles.inventory.list_restore_runs(limit).await {
        Ok(rows) => {
            let dtos: Vec<RestoreRunDto> = rows.into_iter().map(RestoreRunDto::from).collect();
            Json(dtos).into_response()
        }
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list restore runs: {e}"),
        ),
    }
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
                Arc::new(isengard_controller::secrets::SecretsStore::new_locked(inv.clone())),
            ),
        })
    }

    #[tokio::test]
    async fn get_config_returns_defaults_when_unset() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/config")
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
        assert_eq!(parsed["enabled"], false);
        assert_eq!(parsed["interval_secs"], 86400);
        assert_eq!(parsed["retention_keep"], 14);
        assert_eq!(parsed["passphrase_fingerprint"], "");
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let handles = test_handles().await;
        let app = router(handles.clone());
        let body = json!({
            "enabled": true,
            "destination": { "kind": "local", "root": "/tmp/iso", "prefix": "ctrl" },
            "interval_secs": 3600,
            "retention_keep": 7,
            "passphrase_fingerprint": "",
            "passphrase": "test-pass-1"
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["enabled"], true);
        assert_eq!(parsed["interval_secs"], 3600);
        assert_eq!(parsed["retention_keep"], 7);
        assert_eq!(parsed["destination"]["kind"], "local");
        assert_eq!(parsed["destination"]["root"], "/tmp/iso");
        let fp = parsed["passphrase_fingerprint"].as_str().unwrap();
        assert_eq!(fp.len(), 12, "passphrase fingerprint should be set");
    }

    #[tokio::test]
    async fn put_with_zero_interval_rejected() {
        let app = router(test_handles().await);
        let body = json!({
            "enabled": true,
            "destination": { "kind": "none" },
            "interval_secs": 0,
            "retention_keep": 7,
            "passphrase_fingerprint": ""
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_with_zero_retention_rejected() {
        let app = router(test_handles().await);
        let body = json!({
            "enabled": true,
            "destination": { "kind": "none" },
            "interval_secs": 3600,
            "retention_keep": 0,
            "passphrase_fingerprint": ""
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn s3_secret_is_masked_on_get() {
        let handles = test_handles().await;
        let app = router(handles.clone());
        let body = json!({
            "enabled": false,
            "destination": {
                "kind": "s3",
                "endpoint": "https://x.r2.cloudflarestorage.com",
                "region": "auto",
                "bucket": "isengard-backups",
                "prefix": "ctrl",
                "access_key_id": "AK",
                "secret_access_key": "SECRET-VALUE"
            },
            "interval_secs": 3600,
            "retention_keep": 7,
            "passphrase_fingerprint": ""
        });
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["destination"]["secret_access_key"], "***");
        assert_eq!(parsed["destination"]["access_key_id"], "AK");
    }

    #[tokio::test]
    async fn s3_mask_on_put_preserves_existing_secret() {
        let handles = test_handles().await;
        let app = router(handles.clone());
        // First PUT: real secret.
        let first = json!({
            "enabled": false,
            "destination": {
                "kind": "s3",
                "endpoint": "https://x.r2.cloudflarestorage.com",
                "region": "auto",
                "bucket": "iso-b",
                "prefix": "ctrl",
                "access_key_id": "AK",
                "secret_access_key": "ORIGINAL"
            },
            "interval_secs": 3600,
            "retention_keep": 7,
            "passphrase_fingerprint": ""
        });
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(first.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Second PUT: masked secret + changed prefix.
        let second = json!({
            "enabled": true,
            "destination": {
                "kind": "s3",
                "endpoint": "https://x.r2.cloudflarestorage.com",
                "region": "auto",
                "bucket": "iso-b",
                "prefix": "ctrl-new",
                "access_key_id": "AK",
                "secret_access_key": "***"
            },
            "interval_secs": 7200,
            "retention_keep": 10,
            "passphrase_fingerprint": ""
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/backup/config")
                    .header("content-type", "application/json")
                    .body(Body::from(second.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Inspect storage directly: the original secret should still be there.
        let cfg = BackupConfig::load(&handles.inventory).await.unwrap();
        match cfg.destination {
            DestinationConfig::S3 {
                secret_access_key,
                prefix,
                ..
            } => {
                assert_eq!(secret_access_key, "ORIGINAL");
                assert_eq!(prefix, "ctrl-new");
            }
            other => panic!("expected S3 destination, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_now_returns_503_when_runner_not_started() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backup/run-now")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_runs_returns_empty_for_fresh_db() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/runs")
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
        assert!(parsed.is_empty());
    }

    #[tokio::test]
    async fn list_runs_returns_inserted_rows_newest_first() {
        let handles = test_handles().await;
        let now = chrono::Utc::now();
        let id_a = handles.inventory.insert_backup_run(now).await.unwrap();
        let id_b = handles
            .inventory
            .insert_backup_run(now + chrono::Duration::seconds(1))
            .await
            .unwrap();
        handles
            .inventory
            .finish_backup_run_success(id_a, now, "a.db.age", 100)
            .await
            .unwrap();
        handles
            .inventory
            .finish_backup_run_failed(id_b, now + chrono::Duration::seconds(1), "boom")
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/runs?limit=10")
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
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["status"], "failed");
        assert_eq!(parsed[1]["status"], "success");
    }

    // ---------------- endpoint tests ----------------

    #[tokio::test]
    async fn restore_rejects_missing_object_name() {
        let app = router(test_handles().await);
        let body = json!({ "object_name": "", "passphrase": "p", "dry_run": false });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backup/restore")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn restore_rejects_missing_passphrase() {
        let app = router(test_handles().await);
        let body = json!({ "object_name": "snapshot.db.age", "passphrase": "", "dry_run": false });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backup/restore")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn restore_returns_503_when_runner_not_started() {
        let app = router(test_handles().await);
        let body = json!({ "object_name": "snapshot.db.age", "passphrase": "p", "dry_run": false });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backup/restore")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_restore_runs_returns_empty_for_fresh_db() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/restore-runs")
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
        assert!(parsed.is_empty());
    }

    #[tokio::test]
    async fn list_restore_runs_returns_inserted_rows_newest_first() {
        let handles = test_handles().await;
        let now = chrono::Utc::now();
        let id_a = handles
            .inventory
            .insert_restore_run("a.db.age", None, now)
            .await
            .unwrap();
        let id_b = handles
            .inventory
            .insert_restore_run("b.db.age", Some(99), now + chrono::Duration::seconds(1))
            .await
            .unwrap();
        handles
            .inventory
            .finish_restore_run_success(id_a, now, "/tmp/a.bak", 100)
            .await
            .unwrap();
        handles
            .inventory
            .finish_restore_run_failed(id_b, now + chrono::Duration::seconds(1), "boom")
            .await
            .unwrap();

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/restore-runs?limit=5")
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
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["status"], "failed");
        assert_eq!(parsed[0]["source_object"], "b.db.age");
        assert_eq!(parsed[0]["source_backup_run_id"], 99);
        assert_eq!(parsed[1]["status"], "success");
        assert_eq!(parsed[1]["previous_db_backup_path"], "/tmp/a.bak");
    }

    #[tokio::test]
    async fn manifest_returns_404_for_unknown_run() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/backup/runs/9999/manifest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manifest_returns_409_for_non_success_run() {
        let handles = test_handles().await;
        let now = chrono::Utc::now();
        let id = handles.inventory.insert_backup_run(now).await.unwrap();
        handles
            .inventory
            .finish_backup_run_failed(id, now, "boom")
            .await
            .unwrap();
        let app = router(handles);
        let uri = format!("/backup/runs/{}/manifest", id.0);
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn manifest_returns_record_for_success_run() {
        let handles = test_handles().await;
        // Seed a fingerprint via the config.
        let cfg = BackupConfig {
            enabled: true,
            destination: DestinationConfig::Local {
                root: "/tmp/x".into(),
                prefix: "y".into(),
            },
            interval_secs: 3600,
            retention_keep: 7,
            passphrase_fingerprint: "deadbeefcafe".into(),
        };
        cfg.save(&handles.inventory).await.unwrap();

        let now = chrono::Utc::now();
        let id = handles.inventory.insert_backup_run(now).await.unwrap();
        handles
            .inventory
            .finish_backup_run_success(id, now, "snapshot-test.db.age", 4242)
            .await
            .unwrap();

        let app = router(handles);
        let uri = format!("/backup/runs/{}/manifest", id.0);
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["object_name"], "snapshot-test.db.age");
        assert_eq!(parsed["size_bytes"], 4242);
        assert_eq!(parsed["passphrase_fingerprint"], "deadbeefcafe");
    }
}
