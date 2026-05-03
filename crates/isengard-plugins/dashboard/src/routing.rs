//! REST endpoints for routing rules + adapter config (Phase 8h Plan C).
//!
//! See spec §3 in
//! `docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`.
//!
//! PC-T2 wires the rules CRUD endpoints (list/create/update/delete) against
//! `Inventory`. Overrides + adapter-config still return 501 stubs until
//! PC-T3/T4/T5 land.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use isengard_controller::ControllerHandles;
use isengard_storage::{
    InsertRoutingRule, RoutingRule, RoutingRuleId, RoutingRuleSource, RoutingRuleState, TlsMode,
};

pub fn router(handles: Arc<ControllerHandles>) -> Router {
    Router::new()
        .route("/routing/rules", get(list_rules).post(create_rule))
        .route(
            "/routing/rules/{id}",
            patch(update_rule).delete(delete_rule),
        )
        .route("/routing/rules/{id}/overrides", get(list_overrides))
        .route(
            "/routing/rules/{id}/overrides/{field}",
            put(upsert_override),
        )
        .route(
            "/networking/adapter-config/{host_id}/{adapter}",
            get(get_adapter_config).put(upsert_adapter_config),
        )
        .route(
            "/networking/adapter-config/{host_id}/{adapter}/test",
            post(test_adapter_config),
        )
        .with_state(handles)
}

#[derive(serde::Deserialize)]
struct CreateRuleBody {
    fleet: String,
    /// ULID string; parsed into `HostId` before inserting.
    host_id: String,
    stack_id: Option<i64>,
    service_name: String,
    container_port: u16,
    public_hostname: String,
    protocol: String,
    adapter: String,
    tls_mode: TlsMode,
    healthcheck_path: Option<String>,
    healthcheck_interval_secs: Option<u32>,
    auth: Option<String>,
    state: Option<RoutingRuleState>,
    source: Option<RoutingRuleSource>,
}

#[derive(serde::Deserialize)]
struct UpdateRuleBody {
    state: Option<RoutingRuleState>,
}

async fn list_rules(State(handles): State<Arc<ControllerHandles>>) -> Response {
    // For v1 we list across all hosts. Fleet-scoping comes when the
    // active-fleet query param lands.
    let hosts = match handles.inventory.list_hosts().await {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list hosts: {e}"),
            )
                .into_response();
        }
    };
    let mut out: Vec<RoutingRule> = Vec::new();
    for h in hosts {
        if let Ok(mut rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            out.append(&mut rules);
        }
    }
    Json(out).into_response()
}

async fn create_rule(
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<CreateRuleBody>,
) -> Response {
    let host_id = match body.host_id.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "invalid host_id (expected ULID)".to_string(),
            )
                .into_response();
        }
    };
    let stack_id = body.stack_id.map(isengard_storage::StackId);
    let insert = InsertRoutingRule {
        fleet: body.fleet,
        host_id,
        stack_id,
        service_name: body.service_name,
        container_port: body.container_port,
        public_hostname: body.public_hostname,
        protocol: body.protocol,
        adapter: body.adapter,
        tls_mode: body.tls_mode,
        healthcheck_path: body.healthcheck_path,
        healthcheck_interval_secs: body.healthcheck_interval_secs.unwrap_or(10),
        auth: body.auth,
        state: body.state.unwrap_or(RoutingRuleState::Pending),
        source: body.source.unwrap_or(RoutingRuleSource::Ui),
        source_container_id: None,
        source_imported_from: None,
    };
    match handles.inventory.insert_routing_rule(insert).await {
        Ok(rule) => (StatusCode::CREATED, Json(rule)).into_response(),
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            (StatusCode::CONFLICT, format!("hostname conflict: {e}")).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert failed: {e}"),
        )
            .into_response(),
    }
}

async fn update_rule(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpdateRuleBody>,
) -> Response {
    if let Some(state) = body.state {
        if let Err(e) = handles
            .inventory
            .update_routing_rule_state(RoutingRuleId(id), state)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("update failed: {e}"),
            )
                .into_response();
        }
    }
    // Re-fetch and return the updated rule. PC-T3 introduces a direct
    // `get_routing_rule(id)` helper; until then we fan out by host.
    let hosts = handles.inventory.list_hosts().await.unwrap_or_default();
    for h in hosts {
        if let Ok(rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            if let Some(r) = rules.into_iter().find(|r| r.id.0 == id) {
                return Json(r).into_response();
            }
        }
    }
    (StatusCode::NOT_FOUND, "rule not found").into_response()
}

async fn delete_rule(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
) -> Response {
    let hosts = handles.inventory.list_hosts().await.unwrap_or_default();
    let mut found = false;
    for h in hosts {
        if let Ok(rules) = handles.inventory.list_routing_rules_for_host(h.id).await {
            if rules.iter().any(|r| r.id.0 == id) {
                found = true;
                break;
            }
        }
    }
    if !found {
        return (StatusCode::NOT_FOUND, "rule not found").into_response();
    }
    match handles
        .inventory
        .delete_routing_rule(RoutingRuleId(id))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete failed: {e}"),
        )
            .into_response(),
    }
}

async fn list_overrides(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<i64>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "list_overrides: PC-T3").into_response()
}

async fn upsert_override(
    State(_handles): State<Arc<ControllerHandles>>,
    Path((_id, _field)): Path<(i64, String)>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "upsert_override: PC-T3").into_response()
}

async fn get_adapter_config(
    State(_handles): State<Arc<ControllerHandles>>,
    Path((_host_id, _adapter)): Path<(String, String)>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "get_adapter_config: PC-T4").into_response()
}

async fn upsert_adapter_config(
    State(_handles): State<Arc<ControllerHandles>>,
    Path((_host_id, _adapter)): Path<(String, String)>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "upsert_adapter_config: PC-T4").into_response()
}

async fn test_adapter_config(
    State(_handles): State<Arc<ControllerHandles>>,
    Path((_host_id, _adapter)): Path<(String, String)>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "test_adapter_config: PC-T5").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use isengard_controller::bus::EventBus;
    use isengard_storage::{EnrollHost, HostId, Inventory, Journal};
    use tower::ServiceExt;

    async fn test_handles() -> Arc<ControllerHandles> {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let jrnl = Arc::new(Journal::open_in_memory().await.unwrap());
        let bus = Arc::new(EventBus::new());
        let routing = Arc::new(isengard_controller::routing::RoutingPusher::new(
            inv.clone(),
        ));
        Arc::new(ControllerHandles {
            inventory: inv,
            journal: jrnl,
            bus,
            routing,
        })
    }

    async fn seed_host(handles: &ControllerHandles) -> HostId {
        handles
            .inventory
            .enroll_host(EnrollHost {
                fingerprint: "fp-routing".into(),
                hostname: "h-routing".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "27.0".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap()
    }

    async fn seed_rule(
        handles: &ControllerHandles,
        host_id: HostId,
        hostname: &str,
    ) -> RoutingRule {
        handles
            .inventory
            .insert_routing_rule(InsertRoutingRule {
                fleet: "default".into(),
                host_id,
                stack_id: None,
                service_name: "web".into(),
                container_port: 8080,
                public_hostname: hostname.into(),
                protocol: "http".into(),
                adapter: "none".into(),
                tls_mode: TlsMode::Acme,
                healthcheck_path: Some("/healthz".into()),
                healthcheck_interval_secs: 10,
                auth: None,
                state: RoutingRuleState::Pending,
                source: RoutingRuleSource::Ui,
                source_container_id: None,
                source_imported_from: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn list_rules_returns_seeded_rule() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        let rule = seed_rule(&handles, host_id, "blog.example.com").await;

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/routing/rules")
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
        assert_eq!(parsed[0]["public_hostname"], "blog.example.com");
        assert_eq!(parsed[0]["id"], rule.id.0);
    }

    #[tokio::test]
    async fn delete_rule_removes_it_and_returns_204() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        let rule = seed_rule(&handles, host_id, "delete-me.example.com").await;

        let app = router(handles.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/routing/rules/{}", rule.id.0))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let remaining = handles
            .inventory
            .list_routing_rules_for_host(host_id)
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn delete_unknown_id_returns_404() {
        let handles = test_handles().await;
        // Seed a host so list_hosts has something to iterate, but no rules.
        let _ = seed_host(&handles).await;

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/routing/rules/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn adapter_config_get_returns_501_stub() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/networking/adapter-config/01ARZ3NDEKTSV4RRFFQ69G5FAV/tailscale")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
