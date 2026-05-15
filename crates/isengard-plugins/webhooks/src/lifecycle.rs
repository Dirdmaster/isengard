//! Phase 12b: lifecycle-hook event subscriber on the webhooks plugin (#54).
//!
//! Distinct from the 12a webhook subscriber. Listens for the four
//! deployment lifecycle events; for each, looks up the deployment's blue
//! and green container_hooks rows and enqueues `webhook_deliveries` rows
//! with `source='lifecycle'`. The 12a worker drains those alongside
//! webhook deliveries (the worker is source-aware as of T6).

use std::sync::Arc;

use chrono::Utc;
use isengard_core::Event;
use isengard_storage::HostId as StorageHostId;
use isengard_storage::Inventory;
use isengard_storage::deployment::Deployment;
use isengard_storage::webhook::InsertLifecycleDelivery;
use serde::Serialize;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, warn};

/// One of the four lifecycle hook moments. Maps from the bus event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    PreDeploy,
    PostDeploy,
    OnFailure,
}

impl HookKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreDeploy => "pre_deploy",
            Self::PostDeploy => "post_deploy",
            Self::OnFailure => "on_failure",
        }
    }
}

/// Map a bus event kind to the lifecycle hook kind it should fire.
/// `deployment.spinning_up` -> pre_deploy, `deployment.completed` ->
/// post_deploy, `deployment.aborted` and `deployment.failed` ->
/// on_failure. Other kinds return `None` and are ignored.
pub fn lifecycle_kind(event_kind: &str) -> Option<HookKind> {
    match event_kind {
        "deployment.spinning_up" => Some(HookKind::PreDeploy),
        "deployment.completed" => Some(HookKind::PostDeploy),
        "deployment.aborted" | "deployment.failed" => Some(HookKind::OnFailure),
        _ => None,
    }
}

/// Per-event payload POSTed to the configured hook URL.
///
/// Mirrors the `metadata.deployment` shape the agent already emits for
/// every deployment.* event so receivers see consistent fields. Adds the
/// `kind` discriminator and an emit-time `timestamp` so receivers can
/// detect replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecyclePayload<'a> {
    pub kind: &'static str,
    pub deployment_id: &'a str,
    pub host_id: String,
    pub stack_id: i64,
    pub service: &'a str,
    pub blue_digest: &'a str,
    pub green_digest: &'a str,
    pub blue_container: Option<&'a str>,
    pub green_container: Option<&'a str>,
    pub state: &'a str,
    pub timestamp: chrono::DateTime<Utc>,
}

/// Run the subscriber loop until the bus is closed.
pub async fn run(inventory: Arc<Inventory>, mut rx: Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Some(kind) = lifecycle_kind(&event.kind) {
                    if let Err(e) = on_lifecycle(inventory.as_ref(), &event, kind).await {
                        warn!(error = %e, kind = %event.kind, "lifecycle subscriber: enqueue failed");
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    skipped = n,
                    "lifecycle subscriber broadcast lag, events dropped"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                debug!("lifecycle subscriber: broadcast closed; ending task");
                break;
            }
        }
    }
}

/// Process one matched lifecycle event. Pulls the embedded `Deployment`
/// from `event.metadata.deployment`, looks up `container_hooks` rows for
/// blue + green container names, enqueues a delivery for each row that
/// has a URL configured for `kind`.
pub async fn on_lifecycle(
    inventory: &Inventory,
    event: &Event,
    kind: HookKind,
) -> anyhow::Result<()> {
    let Some(dep_value) = event.metadata.get("deployment") else {
        debug!(kind = %event.kind, "lifecycle event missing metadata.deployment; skipping");
        return Ok(());
    };
    let dep: Deployment = match serde_json::from_value(dep_value.clone()) {
        Ok(d) => d,
        Err(e) => {
            warn!(kind = %event.kind, error = %e, "lifecycle event deployment parse failed");
            return Ok(());
        }
    };

    // Each container the deployment knows about: green is the new one,
    // blue is the previous one. Some deployments only have one (e.g.
    // first spin-up has no blue). We look both up because a hook
    // configured on either still applies.
    let names: Vec<&str> = std::iter::once(dep.green_container.as_deref().unwrap_or("").trim())
        .chain(std::iter::once(
            dep.blue_container.as_deref().unwrap_or("").trim(),
        ))
        // Also try the service_name itself: container_hooks is keyed by
        // container_name, but the deployment's green/blue are container
        // ids in production. We additionally try service_name so that a
        // hook configured on the compose service catches.
        .chain(std::iter::once(dep.service_name.as_str()))
        .filter(|n| !n.is_empty())
        .collect();

    let host_storage_id = StorageHostId(dep.host_id.0);
    for name in names {
        let Some(hooks) = inventory.get_container_hooks(host_storage_id, name).await? else {
            continue;
        };
        let url_opt = match kind {
            HookKind::PreDeploy => hooks.pre_deploy_url,
            HookKind::PostDeploy => hooks.post_deploy_url,
            HookKind::OnFailure => hooks.on_failure_url,
        };
        let Some(url) = url_opt else {
            continue;
        };

        let payload = LifecyclePayload {
            kind: kind.as_str(),
            deployment_id: &dep.id,
            host_id: host_storage_id.to_string(),
            stack_id: dep.stack_id.0,
            service: &dep.service_name,
            blue_digest: &dep.blue_digest,
            green_digest: &dep.green_digest,
            blue_container: dep.blue_container.as_deref(),
            green_container: dep.green_container.as_deref(),
            state: dep.state.as_str(),
            timestamp: event.occurred_at,
        };
        let payload_json = serde_json::to_string(&payload)?;

        if let Err(e) = inventory
            .insert_lifecycle_delivery(InsertLifecycleDelivery {
                url: url.clone(),
                secret: hooks.secret.clone(),
                event_kind: event.kind.clone(),
                payload_json,
            })
            .await
        {
            warn!(
                container_name = %name,
                error = %e,
                "insert_lifecycle_delivery failed; lifecycle event lost for this hook",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
    use isengard_storage::stack::{InsertStack, StackSource};
    use isengard_storage::webhook::DeliverySource;
    use isengard_storage::{EnrollHost, UpsertContainerHooks};

    fn ev(kind: &str, dep: &Deployment) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            metadata: serde_json::json!({"deployment": dep}),
            ..Default::default()
        }
    }

    async fn setup() -> (Inventory, isengard_storage::HostId, Deployment) {
        let inv = Inventory::open_in_memory().await.expect("open");
        let host = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0".into(),
                docker_version: "27".into(),
            })
            .await
            .unwrap();
        let stack = inv
            .insert_stack(InsertStack {
                host_id: host,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        let dep_id = "01HX0000000000000000000001".to_string();
        let dep = inv
            .insert_deployment(InsertDeployment {
                id: dep_id,
                host_id: host,
                stack_id: stack,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state: DeploymentState::Pending,
                blue_container: Some("blog-web-blue".into()),
                green_container: Some("blog-web-green".into()),
                blue_digest: "sha256:aaa".into(),
                green_digest: "sha256:bbb".into(),
                previous_digest: None,
                public_hostname: None,
                health_path: None,
                container_port: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        (inv, host, dep)
    }

    #[tokio::test]
    async fn pre_deploy_event_enqueues_lifecycle_delivery() {
        let (inv, host, dep) = setup().await;
        inv.upsert_container_hooks(UpsertContainerHooks {
            host_id: host,
            container_id: "blog-web-green".into(),
            container_name: "web".into(),
            pre_deploy_url: Some("https://h.example/pre".into()),
            post_deploy_url: None,
            on_failure_url: None,
            secret: Some("shh".into()),
        })
        .await
        .unwrap();

        on_lifecycle(
            &inv,
            &ev("deployment.spinning_up", &dep),
            HookKind::PreDeploy,
        )
        .await
        .unwrap();

        let rows = inv
            .list_deliveries_by_source(DeliverySource::Lifecycle, 50)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url.as_deref(), Some("https://h.example/pre"));
        assert_eq!(rows[0].secret.as_deref(), Some("shh"));
        assert_eq!(rows[0].event_kind, "deployment.spinning_up");
    }

    #[tokio::test]
    async fn missing_hook_row_silent_no_op() {
        let (inv, _host, dep) = setup().await;
        // No upsert_container_hooks call.
        on_lifecycle(
            &inv,
            &ev("deployment.spinning_up", &dep),
            HookKind::PreDeploy,
        )
        .await
        .unwrap();
        let rows = inv
            .list_deliveries_by_source(DeliverySource::Lifecycle, 50)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn all_four_event_kinds_map_correctly() {
        assert_eq!(
            lifecycle_kind("deployment.spinning_up"),
            Some(HookKind::PreDeploy)
        );
        assert_eq!(
            lifecycle_kind("deployment.completed"),
            Some(HookKind::PostDeploy)
        );
        assert_eq!(
            lifecycle_kind("deployment.aborted"),
            Some(HookKind::OnFailure)
        );
        assert_eq!(
            lifecycle_kind("deployment.failed"),
            Some(HookKind::OnFailure)
        );
        assert_eq!(lifecycle_kind("deployment.draining"), None);
        assert_eq!(lifecycle_kind("update.success"), None);
    }

    #[tokio::test]
    async fn on_failure_event_picks_on_failure_url_only() {
        let (inv, host, dep) = setup().await;
        inv.upsert_container_hooks(UpsertContainerHooks {
            host_id: host,
            container_id: "blog-web-green".into(),
            container_name: "web".into(),
            pre_deploy_url: Some("https://h.example/pre".into()),
            post_deploy_url: None,
            on_failure_url: Some("https://h.example/fail".into()),
            secret: None,
        })
        .await
        .unwrap();

        on_lifecycle(&inv, &ev("deployment.failed", &dep), HookKind::OnFailure)
            .await
            .unwrap();

        let rows = inv
            .list_deliveries_by_source(DeliverySource::Lifecycle, 50)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url.as_deref(), Some("https://h.example/fail"));
    }

    #[tokio::test]
    async fn malformed_metadata_does_not_panic() {
        let (inv, _host, _dep) = setup().await;
        let mut e = Event {
            kind: "deployment.spinning_up".into(),
            occurred_at: Utc::now(),
            summary: "x".into(),
            ..Default::default()
        };
        e.metadata = serde_json::json!({"deployment": "not a deployment object"});
        // Must not error, must not enqueue.
        on_lifecycle(&inv, &e, HookKind::PreDeploy).await.unwrap();
        let rows = inv
            .list_deliveries_by_source(DeliverySource::Lifecycle, 50)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn no_url_for_kind_skips_enqueue() {
        let (inv, host, dep) = setup().await;
        // Hook row with only post_deploy URL.
        inv.upsert_container_hooks(UpsertContainerHooks {
            host_id: host,
            container_id: "blog-web-green".into(),
            container_name: "web".into(),
            pre_deploy_url: None,
            post_deploy_url: Some("https://h.example/post".into()),
            on_failure_url: None,
            secret: None,
        })
        .await
        .unwrap();
        // Pre-deploy event arrives -> no enqueue.
        on_lifecycle(
            &inv,
            &ev("deployment.spinning_up", &dep),
            HookKind::PreDeploy,
        )
        .await
        .unwrap();
        let rows = inv
            .list_deliveries_by_source(DeliverySource::Lifecycle, 50)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }
}
