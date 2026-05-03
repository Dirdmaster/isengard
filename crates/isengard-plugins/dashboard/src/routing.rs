//! REST endpoints for routing rules + adapter config (Phase 8h Plan C).
//!
//! See spec §3 in
//! `docs/superpowers/specs/2026-05-04-phase-8h-8i-settings-ui-and-atomic-swap-design.md`.
//!
//! All handlers currently return 501 Not Implemented. PC-T2 through PC-T5
//! fill in the real handlers; this scaffold reserves the routes and shape
//! so the dashboard plugin can be wired and tested incrementally.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use isengard_controller::ControllerHandles;

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

async fn list_rules(State(_handles): State<Arc<ControllerHandles>>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "list_rules: PC-T2").into_response()
}

async fn create_rule(State(_handles): State<Arc<ControllerHandles>>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "create_rule: PC-T2").into_response()
}

async fn update_rule(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<i64>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "update_rule: PC-T2").into_response()
}

async fn delete_rule(
    State(_handles): State<Arc<ControllerHandles>>,
    Path(_id): Path<i64>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "delete_rule: PC-T2").into_response()
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
    use isengard_storage::{Inventory, Journal};
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

    #[tokio::test]
    async fn list_rules_returns_501_stub() {
        let app = router(test_handles().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/routing/rules")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
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
