//! REST endpoints for multi-host deployment groups + per-stack parallelism.
//! Phase 10c (T3, refs #50).
//!
//! Routes mounted under `/api/v1`:
//!
//! | Method | Path | Description |
//! | ------ | ---- | ----------- |
//! | GET | `/deployment-groups?stack_id=&state=&limit=` | List groups for a stack (or globally). |
//! | GET | `/deployment-groups/:id` | Single group + embedded deployments. |
//! | POST | `/stacks/:id/deployment-parallelism` | Set parallelism (`"1"`, `"2"`, ..., `"all"`, or null). |
//! | DELETE | `/deployment-groups/:id` | Mark a stuck group aborted. |
//!
//! Filtering by `state`:
//! `pending`, `rolling`, `done`, `aborted`, `failed`, or `active` (rolling +
//! pending). Unknown values fall back to no filter.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use isengard_controller::ControllerHandles;
use isengard_storage::deployment::Deployment;
use isengard_storage::stack::StackId;
use isengard_storage::{DeploymentGroup, DeploymentGroupState};
use serde::{Deserialize, Serialize};

use crate::deployments::DeploymentDto;

#[derive(Debug, Serialize, Deserialize)]
pub struct DeploymentGroupDto {
    pub id: String,
    pub stack_id: i64,
    pub service_name: String,
    pub parallelism: String,
    pub state: String,
    /// Lowercase 32-char hex ids (matches the storage layer's encoding).
    pub target_hosts: Vec<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

impl From<DeploymentGroup> for DeploymentGroupDto {
    fn from(g: DeploymentGroup) -> Self {
        Self {
            id: g.id,
            stack_id: g.stack_id.0,
            service_name: g.service_name,
            parallelism: g.parallelism,
            state: g.state.as_str().to_string(),
            target_hosts: g
                .target_hosts
                .into_iter()
                .map(|h| host_id_hex(h.0.to_bytes()))
                .collect(),
            started_at: g.started_at.to_rfc3339(),
            finished_at: g.finished_at.map(|t| t.to_rfc3339()),
            error: g.error,
        }
    }
}

fn host_id_hex(bytes: [u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeploymentGroupDetailDto {
    #[serde(flatten)]
    pub group: DeploymentGroupDto,
    pub deployments: Vec<DeploymentDto>,
}

#[derive(Debug, Deserialize)]
pub struct ListGroupsQuery {
    pub stack_id: Option<i64>,
    pub state: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SetParallelismBody {
    /// `"1"`, `"2"`, ..., `"N"`, `"all"`, or `null` (clears the override).
    pub parallelism: Option<String>,
}

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/deployment-groups", get(list_groups))
        .route(
            "/deployment-groups/{id}",
            get(get_group).delete(abort_group),
        )
        .route(
            "/stacks/{id}/deployment-parallelism",
            post(set_parallelism).get(get_parallelism),
        )
        .with_state(handles)
}

/// `GET /deployment-groups?stack_id=&state=&limit=`.
async fn list_groups(
    State(handles): State<Arc<ControllerHandles>>,
    Query(q): Query<ListGroupsQuery>,
) -> Result<Json<Vec<DeploymentGroupDto>>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);

    let groups: Vec<DeploymentGroup> = match q.stack_id {
        Some(sid) => handles
            .inventory
            .list_deployment_groups(StackId(sid), limit)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?,
        None => {
            // No global helper exists; iterate every stack.
            let stacks = handles
                .inventory
                .list_stacks(None)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
            let mut all = Vec::new();
            for s in stacks {
                let gs = handles
                    .inventory
                    .list_deployment_groups(s.id, limit)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
                all.extend(gs);
            }
            // Newest first overall.
            all.sort_by_key(|g| std::cmp::Reverse(g.started_at));
            all.truncate(limit as usize);
            all
        }
    };

    let filtered: Vec<DeploymentGroup> = match q.state.as_deref() {
        None => groups,
        Some("active") => groups
            .into_iter()
            .filter(|g| {
                matches!(
                    g.state,
                    DeploymentGroupState::Pending | DeploymentGroupState::Rolling
                )
            })
            .collect(),
        Some(other) => {
            let parsed: Result<DeploymentGroupState, _> = other.parse();
            match parsed {
                Ok(target) => groups.into_iter().filter(|g| g.state == target).collect(),
                Err(_) => groups,
            }
        }
    };

    Ok(Json(filtered.into_iter().map(Into::into).collect()))
}

/// `GET /deployment-groups/:id`.
async fn get_group(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Result<Json<DeploymentGroupDetailDto>, (StatusCode, String)> {
    let group = handles
        .inventory
        .get_deployment_group(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or((StatusCode::NOT_FOUND, format!("group {id} not found")))?;
    let deps: Vec<Deployment> = handles
        .inventory
        .list_deployments_by_group(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(DeploymentGroupDetailDto {
        group: group.into(),
        deployments: deps.into_iter().map(DeploymentDto::from).collect(),
    }))
}

/// `DELETE /deployment-groups/:id`: mark a stuck group as aborted. Idempotent
/// when the group is already terminal: returns 200 with the existing state.
async fn abort_group(
    State(handles): State<Arc<ControllerHandles>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let group = handles
        .inventory
        .get_deployment_group(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or((StatusCode::NOT_FOUND, format!("group {id} not found")))?;
    if group.state.is_terminal() {
        return Ok(StatusCode::OK);
    }
    handles
        .inventory
        .update_deployment_group_state(&id, DeploymentGroupState::Aborted, Some("aborted via REST"))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::ACCEPTED)
}

/// `POST /stacks/:id/deployment-parallelism` body
/// `{"parallelism":"1"|"2"|"N"|"all"|null}`.
async fn set_parallelism(
    State(handles): State<Arc<ControllerHandles>>,
    Path(stack_id): Path<i64>,
    Json(body): Json<SetParallelismBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let val = body.parallelism.as_deref();
    if let Some(v) = val {
        let normalised = v.trim();
        let ok = normalised.eq_ignore_ascii_case("all")
            || normalised.parse::<u32>().map(|n| n >= 1).unwrap_or(false);
        if !ok {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid parallelism value: {v}"),
            ));
        }
    }
    handles
        .inventory
        .set_stack_parallelism(StackId(stack_id), val)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ParallelismDto {
    pub stack_id: i64,
    pub parallelism: Option<String>,
}

/// `GET /stacks/:id/deployment-parallelism`: read back the persisted value.
/// `null` means the stack uses the default (rolling, one host at a time).
async fn get_parallelism(
    State(handles): State<Arc<ControllerHandles>>,
    Path(stack_id): Path<i64>,
) -> Result<Json<ParallelismDto>, (StatusCode, String)> {
    let val = handles
        .inventory
        .get_stack_parallelism(StackId(stack_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(ParallelismDto {
        stack_id,
        parallelism: val,
    }))
}
