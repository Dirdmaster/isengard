//! REST endpoints for blue-green deployments. See spec §10e-rest.
//!
//! Phase 10 Plan B Task 5: read-only `GET /api/v1/deployments?stack_id=&state=`.
//! Phase 10 Plan B Task 10: `POST /api/v1/deployments/:id/abort`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post, put};
use isengard_controller::ControllerHandles;
use isengard_storage::deployment::Deployment;
use isengard_storage::service::ServiceId;
use isengard_storage::stack::StackId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub stack_id: Option<i64>,
    /// `"active"` (default) or `"history"`.
    pub state: Option<String>,
    pub limit: Option<u32>,
    /// Phase 10c (T3 refs #50): when set, returns deployments belonging to
    /// the given group. Mutually exclusive with `stack_id` (group filter
    /// wins).
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentDto {
    pub id: String,
    pub host_id: String,
    pub stack_id: i64,
    pub service_name: String,
    pub strategy: String,
    pub state: String,
    pub blue_container: Option<String>,
    pub green_container: Option<String>,
    pub blue_digest: String,
    pub green_digest: String,
    pub public_hostname: Option<String>,
    pub healthcheck_passed_at: Option<String>,
    pub switched_at: Option<String>,
    pub drained_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Phase 10c (refs #50): set when this deployment is part of a multi-host
    /// rolling group. `None` for single-host (orchestrator-bypass) deploys.
    #[serde(default)]
    pub group_id: Option<String>,
}

impl From<Deployment> for DeploymentDto {
    fn from(d: Deployment) -> Self {
        DeploymentDto {
            id: d.id,
            host_id: d.host_id.0.to_string(),
            stack_id: d.stack_id.0,
            service_name: d.service_name,
            strategy: d.strategy.as_str().to_string(),
            state: d.state.as_str().to_string(),
            blue_container: d.blue_container,
            green_container: d.green_container,
            blue_digest: d.blue_digest,
            green_digest: d.green_digest,
            public_hostname: d.public_hostname,
            healthcheck_passed_at: d.healthcheck_passed_at.map(|t| t.to_rfc3339()),
            switched_at: d.switched_at.map(|t| t.to_rfc3339()),
            drained_at: d.drained_at.map(|t| t.to_rfc3339()),
            finished_at: d.finished_at.map(|t| t.to_rfc3339()),
            error: d.error,
            created_at: d.created_at.to_rfc3339(),
            updated_at: d.updated_at.to_rfc3339(),
            group_id: d.group_id,
        }
    }
}

/// Response body for `POST /deployments/:id/abort`.
///
/// `noop = true` indicates the deployment was already in a terminal state
/// when the request arrived; `reason` carries a human-readable hint such as
/// `deployment_already_terminal: done`. `noop = false` means an
/// `AbortDeployment` message was successfully delivered to the host.
#[derive(Debug, Serialize, Deserialize)]
pub struct AbortResponse {
    pub noop: bool,
    pub reason: Option<String>,
}

/// Per-service deploy strategy override row, surfaced by the
/// `Settings → Deployments` tab. `override_value` is `None` when the service
/// follows the controller default (auto-blue-green if the service is
/// HTTP-routed, otherwise in-place).
#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceDeployStrategyDto {
    pub service_id: i64,
    pub host_id: String,
    pub stack_id: Option<i64>,
    pub stack_name: Option<String>,
    pub service_name: String,
    pub override_value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutOverrideBody {
    /// One of `"auto"`, `"blue-green"`, `"in-place"`. `"auto"` (or `null`)
    /// clears the override.
    pub override_value: Option<String>,
}

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/deployments", get(list_deployments))
        .route("/deployments/{id}/abort", post(abort_deployment))
        .route("/services/deploy-strategy", get(list_service_strategies))
        .route("/services/{id}/deploy-strategy", put(put_service_strategy))
        .with_state(handles)
}

async fn list_deployments(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<DeploymentDto>>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);
    let state_filter = q.state.as_deref().unwrap_or("active");

    // Phase 10c: group_id filter takes precedence. When supplied, return every
    // deployment belonging to the group regardless of state: callers (the
    // group panel UI) want the full picture.
    if let Some(gid) = q.group_id.as_deref() {
        let deps = handles
            .inventory
            .list_deployments_by_group(gid)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
        let dtos: Vec<DeploymentDto> = deps.into_iter().map(DeploymentDto::from).collect();
        return Ok(Json(dtos));
    }

    let deps = match (q.stack_id, state_filter) {
        (Some(sid), "active") => {
            // `list_deployments_by_stack` returns the most recent rows for
            // a stack. Filter in-memory: small lists (<= limit, capped 200).
            let all = handles
                .inventory
                .list_deployments_by_stack(StackId(sid), limit)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
            all.into_iter()
                .filter(|d| !d.state.is_terminal())
                .collect::<Vec<_>>()
        }
        (Some(sid), "history") => {
            let all = handles
                .inventory
                .list_deployments_by_stack(StackId(sid), limit)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
            all.into_iter()
                .filter(|d| d.state.is_terminal())
                .collect::<Vec<_>>()
        }
        (Some(_), other) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown state filter: {other}"),
            ));
        }
        (None, _) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "stack_id query param required".into(),
            ));
        }
    };

    Ok(Json(deps.into_iter().map(DeploymentDto::from).collect()))
}

/// `POST /deployments/:id/abort`: request that the agent abort an
/// in-flight deployment.
///
/// Returns `202 Accepted` with `{ noop: false }` once the
/// `AbortDeployment` proto has been handed to the routing pusher. If the
/// deployment is already terminal, returns `200 OK` with
/// `{ noop: true, reason: ... }`. If the host is currently disconnected,
/// returns `503 Service Unavailable`.
async fn abort_deployment(
    State(handles): State<Arc<ControllerHandles>>,
    Path(deployment_id): Path<String>,
) -> Result<(StatusCode, Json<AbortResponse>), (StatusCode, String)> {
    let dep = handles
        .inventory
        .get_deployment(&deployment_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("deployment {deployment_id} not found"),
        ))?;

    if dep.state.is_terminal() {
        return Ok((
            StatusCode::OK,
            Json(AbortResponse {
                noop: true,
                reason: Some(format!(
                    "deployment_already_terminal: {}",
                    dep.state.as_str()
                )),
            }),
        ));
    }

    let msg = isengard_proto::pb::ControllerMessage {
        payload: Some(
            isengard_proto::pb::controller_message::Payload::AbortDeployment(
                isengard_proto::pb::AbortDeployment {
                    deployment_id: deployment_id.clone(),
                },
            ),
        ),
    };

    handles
        .routing
        .send_message_to_host(dep.host_id, msg)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("agent offline: {e}"),
            )
        })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(AbortResponse {
            noop: false,
            reason: None,
        }),
    ))
}

/// `GET /services/deploy-strategy`: list every service with its current
/// per-service strategy override (or `None` when following the default).
async fn list_service_strategies(
    State(handles): State<Arc<ControllerHandles>>,
) -> Result<Json<Vec<ServiceDeployStrategyDto>>, (StatusCode, String)> {
    let services = handles
        .inventory
        .list_services(None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;

    let mut dtos = Vec::with_capacity(services.len());
    for s in services {
        let stack_name = match s.stack_id {
            Some(sid) => handles
                .inventory
                .get_stack(sid)
                .await
                .ok()
                .flatten()
                .map(|st| st.name),
            None => None,
        };
        dtos.push(ServiceDeployStrategyDto {
            service_id: s.id.0,
            host_id: s.host_id.to_string(),
            stack_id: s.stack_id.map(|s| s.0),
            stack_name,
            service_name: s.name,
            override_value: s.deploy_strategy_override,
        });
    }
    Ok(Json(dtos))
}

/// `PUT /services/{id}/deploy-strategy`: set or clear the per-service
/// strategy override. `"auto"` (or `null`) clears; `"blue-green"` and
/// `"in-place"` are the only other accepted values.
async fn put_service_strategy(
    State(handles): State<Arc<ControllerHandles>>,
    Path(service_id): Path<i64>,
    Json(body): Json<PutOverrideBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    if let Some(v) = body.override_value.as_deref() {
        if !["auto", "blue-green", "in-place"].contains(&v) {
            return Err((StatusCode::BAD_REQUEST, format!("invalid override: {v}")));
        }
    }
    handles
        .inventory
        .set_service_deploy_strategy_override(
            ServiceId(service_id),
            body.override_value.as_deref().filter(|v| *v != "auto"),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
}
