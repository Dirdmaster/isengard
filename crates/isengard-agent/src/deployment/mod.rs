//! Blue-green deployment driver. See spec
//! `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`.

pub mod driver;
pub mod eligibility;
pub mod healthcheck;

use crate::deployment::driver::{Driver, RealDriverDeps};
use crate::deployment::eligibility::{ContainerSpec, Decision, classify};
use crate::proxy::ProxyState;
use anyhow::Result;
use isengard_core::EventEmitter;
use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::host::HostId;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::StackId;
use std::sync::Arc;
use ulid::Ulid;

/// Typed trigger consumed by the [`DeploymentSupervisor`]. Built by the
/// dispatcher (Task 6) from the cross-crate [`isengard_core::UpdateTriggerInfo`]
/// after a routing-rule lookup, so that by the time we reach the supervisor
/// the `public_hostname` / `health_path` / `container_port` are already
/// resolved (or known to be absent).
#[derive(Debug, Clone)]
pub struct UpdateTrigger {
    pub container_id: String,
    pub host_id: HostId,
    pub stack_id: StackId,
    pub service_name: String,
    pub blue_digest: String,
    pub green_digest: String,
    pub image_ref: String,
    pub public_hostname: Option<String>,
    pub container_port: Option<u16>,
    pub health_path: Option<String>,
    pub has_healthcheck: bool,
    pub rw_volume_mounts: Vec<String>,
    pub label_strategy: Option<String>,
}

/// Result of [`DeploymentSupervisor::handle_update_trigger`]. The dispatcher
/// uses this to decide whether the updater plugin still needs to perform an
/// in-place recreate or if the supervisor has already taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorOutcome {
    /// Inserted a `Deployment` row + spawned a [`Driver`] task. The updater
    /// must NOT recreate — the driver owns the lifecycle now.
    BlueGreenSpawned { deployment_id: String },
    /// Container is not eligible for blue-green; updater should run the
    /// existing in-place recreate path.
    InPlaceForUpdater,
    /// A non-terminal deployment for this `(host, service)` already exists.
    /// Skip — no row inserted, no driver spawned.
    AlreadyInFlight,
}

/// Owns the blue-green decision + Driver-spawn pipeline. One per agent
/// process, shared across plugins via `Arc`. See spec §`mod.rs`.
pub struct DeploymentSupervisor {
    inventory: Inventory,
    docker: Arc<bollard::Docker>,
    proxy_state: ProxyState,
    emitter: Arc<dyn EventEmitter>,
}

impl DeploymentSupervisor {
    pub fn new(
        inventory: Inventory,
        docker: Arc<bollard::Docker>,
        proxy_state: ProxyState,
        emitter: Arc<dyn EventEmitter>,
    ) -> Self {
        Self {
            inventory,
            docker,
            proxy_state,
            emitter,
        }
    }

    /// Mark every non-terminal deployment row for `host_id` as `Failed` with
    /// `error = "agent_restarted_during_deployment"`. v1 does not attempt
    /// to resume; the driver task is gone, so the safest action is to
    /// surface the orphan and let the next updater poll re-trigger.
    /// Returns the number of rows updated.
    pub async fn reconcile_orphans(&self, host_id: HostId) -> Result<u64> {
        let n = self
            .inventory
            .mark_orphan_deployments_failed(host_id, "agent_restarted_during_deployment")
            .await?;
        Ok(n)
    }

    /// Classify the trigger; either spawn a [`Driver`] (BlueGreen), bail
    /// with `InPlaceForUpdater` (so the updater plugin handles it), or
    /// short-circuit with `AlreadyInFlight` if a deployment for this
    /// `(host, service)` is already running.
    ///
    /// Returns quickly: the driver runs in a detached `tokio::spawn`.
    pub async fn handle_update_trigger(&self, trigger: UpdateTrigger) -> Result<SupervisorOutcome> {
        // Dedupe first — a second updater tick should not race a driver
        // already mid-deploy for the same service.
        let in_flight = self
            .inventory
            .list_in_flight_for_service(trigger.host_id, &trigger.service_name)
            .await?;
        if !in_flight.is_empty() {
            return Ok(SupervisorOutcome::AlreadyInFlight);
        }

        let mounts = trigger.rw_volume_mounts.clone();
        let spec = ContainerSpec {
            has_routing_rule: trigger.public_hostname.is_some(),
            has_healthcheck: trigger.has_healthcheck,
            rw_volume_mounts: &mounts,
            label_strategy: trigger.label_strategy.as_deref(),
        };
        match classify(&spec) {
            Decision::InPlace { .. } => Ok(SupervisorOutcome::InPlaceForUpdater),
            Decision::BlueGreen => {
                let id = Ulid::new().to_string();
                let row = self
                    .inventory
                    .insert_deployment(InsertDeployment {
                        id: id.clone(),
                        host_id: trigger.host_id,
                        stack_id: trigger.stack_id,
                        service_name: trigger.service_name.clone(),
                        strategy: DeployStrategy::BlueGreen,
                        state: DeploymentState::Pending,
                        blue_container: Some(trigger.container_id.clone()),
                        green_container: None,
                        blue_digest: trigger.blue_digest.clone(),
                        green_digest: trigger.green_digest.clone(),
                        public_hostname: trigger.public_hostname.clone(),
                        health_path: trigger.health_path.clone(),
                        container_port: trigger.container_port.map(|p| p as i64),
                        metadata_json: None,
                    })
                    .await?;
                let deps = Arc::new(RealDriverDeps {
                    docker: self.docker.clone(),
                    proxy_state: self.proxy_state.clone(),
                });
                let driver = Driver::new(row, deps, self.inventory.clone(), self.emitter.clone());
                tokio::spawn(driver.run());
                Ok(SupervisorOutcome::BlueGreenSpawned { deployment_id: id })
            }
        }
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use isengard_core::NoopEmitter;
    use isengard_storage::host::EnrollHost;
    use isengard_storage::stack::{InsertStack, StackSource};

    /// In-memory inventory + supervisor + (host, stack) IDs for the
    /// supervisor tests. Matches the EnrollHost shape used by T4's
    /// `setup_inventory_and_row` in `driver.rs`.
    async fn setup() -> (DeploymentSupervisor, HostId, StackId, Inventory) {
        let inv = Inventory::open_in_memory().await.unwrap();
        let host_id = inv
            .enroll_host(EnrollHost {
                fingerprint: "fp".into(),
                hostname: "h".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.1.0".into(),
                docker_version: "24.0".into(),
                fleet: "default".into(),
            })
            .await
            .unwrap();
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: "blog".into(),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        // bollard's `connect_with_local_defaults` is lazy — it does not
        // open the socket until the first request, so this works in any
        // CI environment regardless of whether Docker is actually
        // running. The supervisor tests below never reach the docker
        // path: they bail at the dedupe check, the InPlace classify
        // branch, or use only the inventory.
        let docker = Arc::new(bollard::Docker::connect_with_local_defaults().unwrap());
        let proxy_state = ProxyState::new();
        let sup =
            DeploymentSupervisor::new(inv.clone(), docker, proxy_state, Arc::new(NoopEmitter));
        (sup, host_id, stack_id, inv)
    }

    /// `blue_green=false` produces a trigger with no public_hostname,
    /// which the eligibility classifier maps to InPlace(NoRoutingRule).
    fn trigger(host_id: HostId, stack_id: StackId, blue_green: bool) -> UpdateTrigger {
        UpdateTrigger {
            container_id: "c-blue".into(),
            host_id,
            stack_id,
            service_name: "web".into(),
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            image_ref: "blog/web:1.3.0".into(),
            public_hostname: if blue_green {
                Some("blog.test".into())
            } else {
                None
            },
            container_port: Some(8080),
            health_path: Some("/healthz".into()),
            has_healthcheck: blue_green,
            rw_volume_mounts: vec![],
            label_strategy: None,
        }
    }

    #[tokio::test]
    async fn classifies_in_place_when_no_routing_rule() {
        let (sup, host, stack, _inv) = setup().await;
        let outcome = sup
            .handle_update_trigger(trigger(host, stack, false))
            .await
            .unwrap();
        assert_eq!(outcome, SupervisorOutcome::InPlaceForUpdater);
    }

    #[tokio::test]
    async fn dedupes_when_in_flight_deployment_exists() {
        let (sup, host, stack, inv) = setup().await;
        // Pre-insert a SpinningUp row for service "web" so the dedupe
        // branch fires before classify is even consulted.
        inv.insert_deployment(InsertDeployment {
            id: Ulid::new().to_string(),
            host_id: host,
            stack_id: stack,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::SpinningUp,
            blue_container: Some("c-blue".into()),
            green_container: None,
            blue_digest: "sha256:old".into(),
            green_digest: "sha256:newer".into(),
            public_hostname: Some("blog.test".into()),
            health_path: None,
            container_port: Some(8080),
            metadata_json: None,
        })
        .await
        .unwrap();

        let outcome = sup
            .handle_update_trigger(trigger(host, stack, true))
            .await
            .unwrap();
        assert_eq!(outcome, SupervisorOutcome::AlreadyInFlight);
    }

    #[tokio::test]
    async fn reconcile_orphans_marks_non_terminal_as_failed() {
        let (sup, host, stack, inv) = setup().await;
        let d = inv
            .insert_deployment(InsertDeployment {
                id: Ulid::new().to_string(),
                host_id: host,
                stack_id: stack,
                service_name: "web".into(),
                strategy: DeployStrategy::BlueGreen,
                state: DeploymentState::SpinningUp,
                blue_container: Some("c-blue".into()),
                green_container: None,
                blue_digest: "sha256:old".into(),
                green_digest: "sha256:newer".into(),
                public_hostname: Some("blog.test".into()),
                health_path: None,
                container_port: Some(8080),
                metadata_json: None,
            })
            .await
            .unwrap();

        let n = sup.reconcile_orphans(host).await.unwrap();
        assert_eq!(n, 1);
        let after = inv.get_deployment(&d.id).await.unwrap().unwrap();
        assert_eq!(after.state, DeploymentState::Failed);
        assert_eq!(
            after.error.as_deref(),
            Some("agent_restarted_during_deployment")
        );
    }
}
