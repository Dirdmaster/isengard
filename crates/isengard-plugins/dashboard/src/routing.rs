//! REST endpoints for routing rules + adapter config.
//!
//! See spec §3 in
//! `docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`.
//!
//! PC-T2 wires the rules CRUD endpoints (list/create/update/delete) against
//! `Inventory`. PC-T3 wires per-field overrides (list + upsert). PC-T4 wires
//! adapter-config GET/PUT against `Inventory`. PC-T5 wires the /test endpoint
//! with per-adapter validation logic (none/cf-tunnel/tailscale).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use isengard_controller::ControllerHandles;
use isengard_storage::{
    InsertRoutingRule, RoutingRuleId, RoutingRuleSource, RoutingRuleState, TlsMode,
    UpsertAdapterConfig,
};

/// Builds the axum router for this resource.
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
/// CreateRuleBody.
struct CreateRuleBody {
    /// ULID string; parsed into `HostId` before inserting.
    host_id: String,
    /// `stack_id` field.
    stack_id: Option<i64>,
    /// `service_name` field.
    service_name: String,
    /// `container_port` field.
    container_port: u16,
    /// `public_hostname` field.
    public_hostname: String,
    /// `protocol` field.
    protocol: String,
    /// `adapter` field.
    adapter: String,
    /// `tls_mode` field.
    tls_mode: TlsMode,
    /// `healthcheck_path` field.
    healthcheck_path: Option<String>,
    /// `healthcheck_interval_secs` field.
    healthcheck_interval_secs: Option<u32>,
    /// `auth` field.
    auth: Option<String>,
    /// `state` field.
    state: Option<RoutingRuleState>,
    /// `source` field.
    source: Option<RoutingRuleSource>,
}

#[derive(serde::Deserialize)]
/// UpdateRuleBody.
struct UpdateRuleBody {
    /// `state` field.
    state: Option<RoutingRuleState>,
}

/// `GET` handler for rules.
async fn list_rules(State(handles): State<Arc<ControllerHandles>>) -> Response {
    // Single cluster-wide query.
    match handles.inventory.list_all_routing_rules().await {
        Ok(rules) => Json(rules).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list rules: {e}"),
        )
            .into_response(),
    }
}

/// `POST` handler for rule.
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
        // The agent's apply_config does not filter on rule state; once the
        // rule is in storage it's eligible to route. Default to Active so
        // the operator-facing state column reflects reality. Callers that
        // genuinely want a staged pre-deployment rule can pass Pending.
        state: body.state.unwrap_or(RoutingRuleState::Active),
        source: body.source.unwrap_or(RoutingRuleSource::Ui),
        source_container_id: None,
        source_imported_from: None,
    };
    match handles.inventory.insert_routing_rule(insert).await {
        Ok(rule) => {
            // Push immediately. Without this the agent only learns of the
            // new rule on its next sync reconnect or via the safety-net
            // sweeper, both of which are minutes away. Failure here is
            // logged but does not fail the create: storage already has the
            // rule, the next push picks it up.
            if let Err(e) = handles.routing.push_to_all_hosts().await {
                tracing::warn!(error = %e, "post-create proxy_config push failed");
            }
            (StatusCode::CREATED, Json(rule)).into_response()
        }
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

/// `PUT` handler for rule.
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
        if let Err(e) = handles.routing.push_to_all_hosts().await {
            tracing::warn!(error = %e, "post-update proxy_config push failed");
        }
    }
    match handles.inventory.get_routing_rule(RoutingRuleId(id)).await {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get after update: {e}"),
        )
            .into_response(),
    }
}

/// `DELETE` handler for rule.
async fn delete_rule(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
) -> Response {
    match handles.inventory.get_routing_rule(RoutingRuleId(id)).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("lookup before delete: {e}"),
            )
                .into_response();
        }
    }
    match handles
        .inventory
        .delete_routing_rule(RoutingRuleId(id))
        .await
    {
        Ok(()) => {
            if let Err(e) = handles.routing.push_to_all_hosts().await {
                tracing::warn!(error = %e, "post-delete proxy_config push failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET` handler for overrides.
async fn list_overrides(
    Path(id): Path<i64>,
    State(handles): State<Arc<ControllerHandles>>,
) -> Response {
    match handles
        .inventory
        .list_routing_rule_overrides(RoutingRuleId(id))
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list overrides: {e}"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
/// UpsertOverrideBody.
struct UpsertOverrideBody {
    /// `value_json` field.
    value_json: serde_json::Value,
}

/// `upsert_override`.
async fn upsert_override(
    Path((id, field)): Path<(i64, String)>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpsertOverrideBody>,
) -> Response {
    match handles
        .inventory
        .upsert_routing_rule_override(RoutingRuleId(id), &field, body.value_json.clone())
        .await
    {
        Ok(()) => Json(serde_json::json!({
            "routing_rule_id": id,
            "field": field,
            "value_json": body.value_json,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("upsert override: {e}"),
        )
            .into_response(),
    }
}

/// `GET` handler for adapter config.
async fn get_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
) -> Response {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    match handles
        .inventory
        .get_adapter_config(host_id, &adapter)
        .await
    {
        Ok(Some(cfg)) => Json(cfg).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no adapter config for host+adapter").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get adapter_config: {e}"),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
/// UpsertAdapterBody.
struct UpsertAdapterBody {
    /// `config_json` field.
    config_json: serde_json::Value,
    /// `enabled` field.
    enabled: bool,
}

/// `upsert_adapter_config`.
async fn upsert_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
    Json(body): Json<UpsertAdapterBody>,
) -> Response {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    let ins = UpsertAdapterConfig {
        host_id,
        adapter: adapter.clone(),
        config_json: body.config_json.clone(),
        enabled: body.enabled,
    };
    if let Err(e) = handles.inventory.upsert_adapter_config(ins).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("upsert adapter_config: {e}"),
        )
            .into_response();
    }
    match handles
        .inventory
        .get_adapter_config(host_id, &adapter)
        .await
    {
        Ok(Some(cfg)) => Json(cfg).into_response(),
        Ok(None) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "upsert succeeded but get returned None",
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get after upsert: {e}"),
        )
            .into_response(),
    }
}

/// `test_adapter_config`.
async fn test_adapter_config(
    Path((host_id_str, adapter)): Path<(String, String)>,
    State(handles): State<Arc<ControllerHandles>>,
) -> Response {
    let host_id = match host_id_str.parse::<ulid::Ulid>() {
        Ok(u) => isengard_storage::HostId(u),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid host_id").into_response(),
    };
    let cfg = match handles
        .inventory
        .get_adapter_config(host_id, &adapter)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "no adapter config to test").into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read adapter_config: {e}"),
            )
                .into_response();
        }
    };

    let result = match adapter.as_str() {
        "none" => serde_json::json!({ "ok": true, "detail": null }),
        "cf-tunnel" => {
            let token = cfg
                .config_json
                .get("api_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let account_id = cfg
                .config_json
                .get("account_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if token.is_empty() || account_id.is_empty() {
                serde_json::json!({
                    "ok": false,
                    "error": "missing api_token or account_id",
                })
            } else {
                let url = format!(
                    "https://api.cloudflare.com/client/v4/accounts/{}/cfd_tunnel?per_page=1",
                    account_id
                );
                // 30s timeout: a stuck CF API connection would otherwise hang
                // the dashboard request handler indefinitely (no per-request
                // budget on `reqwest::Client::new()`'s default config).
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                match client.get(&url).bearer_auth(token).send().await {
                    Ok(resp) if resp.status().is_success() => serde_json::json!({
                        "ok": true,
                        "detail": null,
                    }),
                    Ok(resp) => serde_json::json!({
                        "ok": false,
                        "error": format!("CF API returned {}", resp.status()),
                    }),
                    Err(e) => serde_json::json!({
                        "ok": false,
                        "error": format!("CF API request failed: {e}"),
                    }),
                }
            }
        }
        "tailscale" => serde_json::json!({
            "ok": true,
            "detail": "tailscale runtime status surfaces via agent heartbeat",
        }),
        other => serde_json::json!({
            "ok": false,
            "error": format!("unknown adapter: {other}"),
        }),
    };

    Json(result).into_response()
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
    use isengard_storage::{EnrollHost, HostId, Inventory, Journal, RoutingRule};
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
    async fn list_overrides_returns_empty_for_new_rule() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        let rule = seed_rule(&handles, host_id, "blog.example.com").await;

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/routing/rules/{}/overrides", rule.id.0))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let overrides: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(overrides.is_empty());
    }

    #[tokio::test]
    async fn upsert_override_then_list_returns_value() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        let rule = seed_rule(&handles, host_id, "blog.example.com").await;

        let app = router(handles);
        let put_body = serde_json::json!({"value_json": "manual"}).to_string();
        let put_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/routing/rules/{}/overrides/tls_mode", rule.id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(put_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_resp.status(), StatusCode::OK);

        let list_resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/routing/rules/{}/overrides", rule.id.0))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(list_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let overrides: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0]["field"], "tls_mode");
        assert_eq!(overrides[0]["value_json"], "manual");
    }

    #[tokio::test]
    async fn get_adapter_config_returns_404_when_not_set() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;

        let app = router(handles);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/networking/adapter-config/{}/cf-tunnel", host_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn upsert_then_get_adapter_config_returns_blob() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        let path = format!("/networking/adapter-config/{}/cf-tunnel", host_id);

        let app = router(handles);
        let put_body = serde_json::json!({
            "config_json": {
                "api_token": "secret",
                "account_id": "acct-1",
                "zone_id": "zone-1",
            },
            "enabled": true,
        })
        .to_string();
        let put_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(&path)
                    .header("content-type", "application/json")
                    .body(Body::from(put_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_resp.status(), StatusCode::OK);

        let get_resp = app
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(get_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let cfg: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cfg["adapter"], "cf-tunnel");
        assert_eq!(cfg["enabled"], true);
        assert_eq!(cfg["config_json"]["api_token"], "secret");
        assert_eq!(cfg["config_json"]["account_id"], "acct-1");
        assert_eq!(cfg["config_json"]["zone_id"], "zone-1");
    }

    #[tokio::test]
    async fn test_adapter_config_for_none_returns_ok_true() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;
        handles
            .inventory
            .upsert_adapter_config(UpsertAdapterConfig {
                host_id,
                adapter: "none".into(),
                config_json: serde_json::json!({}),
                enabled: true,
            })
            .await
            .unwrap();

        let app = router(handles);
        let path = format!("/networking/adapter-config/{}/none/test", host_id);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
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
        assert_eq!(parsed["ok"], true);
    }

    #[tokio::test]
    async fn test_adapter_config_for_unconfigured_returns_404() {
        let handles = test_handles().await;
        let host_id = seed_host(&handles).await;

        let app = router(handles);
        let path = format!("/networking/adapter-config/{}/cf-tunnel/test", host_id);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
