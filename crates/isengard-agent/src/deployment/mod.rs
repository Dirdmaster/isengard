//! Blue-green deployment driver. See spec
//! `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`.

pub mod driver;
pub mod eligibility;
pub mod healthcheck;

use crate::deployment::driver::{Driver, RealDriverDeps};
use crate::deployment::eligibility::{ContainerSpec, Decision, classify};
use crate::proxy::ProxyState;
use anyhow::Result;
use async_trait::async_trait;
use isengard_core::policy::{FailureHandling, PolicyContext, PolicyScopeType, resolve_policy};
use isengard_core::{
    DispatchOutcome, EventEmitter, PolicyLoader, UpdateDispatcher, UpdateTriggerInfo,
};
use isengard_storage::deployment::{DeployStrategy, DeploymentState, InsertDeployment};
use isengard_storage::host::HostId;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::StackId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use tokio_util::sync::CancellationToken;
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
    /// Live `CancellationToken`s keyed by deployment id. Inserted before
    /// spawning a [`Driver`], removed when the driver task returns. Plan
    /// B 10f's [`Self::handle_abort`] looks up the token and fires it,
    /// driving the in-flight driver into its abort path.
    abort_tokens: Arc<StdRwLock<HashMap<String, CancellationToken>>>,
    /// Phase 9F: optional policy loader. When wired, the supervisor
    /// resolves a `ResolvedPolicy` per trigger and consults
    /// `on_failure` to decide whether to seed `previous_digest` and
    /// what failure-handling mode to pass to the driver. `None` keeps
    /// every trigger on the legacy Notify-by-default path.
    policy_loader: Option<Arc<dyn PolicyLoader>>,
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
            abort_tokens: Arc::new(StdRwLock::new(HashMap::new())),
            policy_loader: None,
        }
    }

    /// Phase 9F: install a `PolicyLoader` so the supervisor can resolve
    /// `on_failure` per trigger. Without one, every deployment defaults
    /// to Notify (existing behaviour). With one, deployments whose
    /// resolved policy says `Rollback` get `previous_digest` populated
    /// and the driver picks up the rollback branch.
    pub fn with_policy_loader(mut self, loader: Arc<dyn PolicyLoader>) -> Self {
        self.policy_loader = Some(loader);
        self
    }

    /// Cancel the [`CancellationToken`] for `deployment_id`, if one is
    /// registered. Returns `true` if a token was found and cancelled,
    /// `false` if no in-flight deployment matches (already finished, never
    /// existed, or wrong host). The driver task observes the cancel at
    /// the next state-machine race point. Plan B 10f.
    pub async fn handle_abort(&self, deployment_id: &str) -> bool {
        let tokens = self.abort_tokens.read().unwrap();
        if let Some(token) = tokens.get(deployment_id) {
            token.cancel();
            true
        } else {
            false
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
        if n > 0 {
            tracing::warn!(
                host_id = %host_id,
                orphans = n,
                "deployment supervisor: marked orphan deployments as failed at startup"
            );
        }
        Ok(n)
    }

    /// Phase 9F: resolve the failure-handling policy for this trigger.
    /// Falls back to `FailureHandling::Notify` (the design's default) on
    /// any error so a slow or broken policy DAO never blocks a deploy.
    /// Returns the resolved enum AND a boolean indicating whether a
    /// policy loader was actually consulted (so callers can distinguish
    /// "loader said Notify" from "no loader wired").
    async fn resolve_failure_handling(&self, trigger: &UpdateTrigger) -> (FailureHandling, bool) {
        let Some(loader) = self.policy_loader.as_ref() else {
            return (FailureHandling::Notify, false);
        };
        // Fleet name lookup. `None` is fine: the resolver simply skips
        // fleet-scoped rows. `Err` is treated the same as None: we
        // never block on policy evaluation.
        let fleet = loader.fleet_for(trigger.host_id.0).await.ok().flatten();
        let rows = match loader.list().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    host_id = %trigger.host_id,
                    service = %trigger.service_name,
                    error = %e,
                    "supervisor: policy_loader.list failed; defaulting to Notify"
                );
                return (FailureHandling::Notify, true);
            }
        };
        // Project the snapshot down to the resolver's tuple shape. The
        // resolver re-sorts by rank internally so input order is
        // irrelevant.
        let projected: Vec<(PolicyScopeType, &str, &isengard_core::policy::Policy)> = rows
            .iter()
            .map(|r| (r.scope_type, r.scope_key.as_str(), &r.body))
            .collect();
        let host_hex = trigger.host_id.0.to_string();
        let ctx = PolicyContext {
            fleet: fleet.as_deref(),
            // Stack scope_key uses the bare stack name; the supervisor
            // doesn't have it directly, so we look it up best-effort
            // via the inventory. Failure here just means stack-scoped
            // rows don't apply, which is harmless (less specific
            // scopes still resolve).
            stack: None,
            service: Some(trigger.service_name.as_str()),
            host_id_hex: Some(host_hex.as_str()),
            container_name: None,
        };
        let resolved = resolve_policy(&projected, &ctx);
        (resolved.on_failure, true)
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

        // Consult the stored service-level override as a fallback. Container
        // label always wins; the stored override is for cases where the user
        // set a strategy via the UI/CLI without redeploying with new labels.
        // Lookup is best-effort — a missing service row or DB hiccup must
        // not block the deploy, so we swallow errors and treat them as "no
        // override".
        let stored_override = self
            .inventory
            .get_service_by_name(
                trigger.host_id,
                Some(trigger.stack_id),
                &trigger.service_name,
            )
            .await
            .ok()
            .flatten()
            .and_then(|s| s.deploy_strategy_override);
        let effective_strategy: Option<String> = trigger.label_strategy.clone().or(stored_override);

        let mounts = trigger.rw_volume_mounts.clone();
        let spec = ContainerSpec {
            has_routing_rule: trigger.public_hostname.is_some(),
            has_healthcheck: trigger.has_healthcheck,
            rw_volume_mounts: &mounts,
            label_strategy: effective_strategy.as_deref(),
        };
        match classify(&spec) {
            Decision::InPlace { .. } => Ok(SupervisorOutcome::InPlaceForUpdater),
            Decision::BlueGreen => {
                // Phase 9F: resolve the failure-handling policy ONCE
                // before insert + spawn so we can both seed
                // `previous_digest` and tell the driver what mode to
                // operate in.
                let (failure_handling, _resolved) = self.resolve_failure_handling(&trigger).await;
                let previous_digest = match failure_handling {
                    FailureHandling::Rollback => Some(trigger.blue_digest.clone()),
                    FailureHandling::Keep | FailureHandling::Notify => None,
                };

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
                        previous_digest,
                    })
                    .await?;
                // Register an abort token before spawn so the dashboard
                // (via AbortDeployment) can cancel mid-flight. The driver
                // races this token at every state-machine edge.
                let abort_token = CancellationToken::new();
                {
                    let mut tokens = self.abort_tokens.write().unwrap();
                    tokens.insert(id.clone(), abort_token.clone());
                }
                let proxy_events_rx = self.proxy_state.proxy_events.subscribe();

                let deps = Arc::new(RealDriverDeps {
                    docker: self.docker.clone(),
                    proxy_state: self.proxy_state.clone(),
                });
                let driver = Driver::new(row, deps, self.inventory.clone(), self.emitter.clone())
                    .with_abort_token(abort_token)
                    .with_proxy_events(proxy_events_rx)
                    .with_failure_policy(failure_handling);
                tracing::info!(
                    deployment_id = %id,
                    host_id = %trigger.host_id,
                    service = %trigger.service_name,
                    image = %trigger.image_ref,
                    "deployment supervisor: blue-green driver spawned"
                );

                let tokens_for_cleanup = self.abort_tokens.clone();
                let id_for_cleanup = id.clone();
                tokio::spawn(async move {
                    driver.run().await;
                    tokens_for_cleanup.write().unwrap().remove(&id_for_cleanup);
                });
                Ok(SupervisorOutcome::BlueGreenSpawned { deployment_id: id })
            }
        }
    }
}

impl DeploymentSupervisor {
    /// Test-only: pre-register an abort token so integration tests can
    /// exercise [`Self::handle_abort`] without spinning up a full driver.
    /// The leading underscore + `#[doc(hidden)]` mark it as not part of
    /// the public surface; production callers should let the BlueGreen
    /// branch of [`Self::handle_update_trigger`] register tokens.
    #[doc(hidden)]
    pub fn _test_register_abort_token(&self, id: &str, token: CancellationToken) {
        self.abort_tokens
            .write()
            .unwrap()
            .insert(id.to_string(), token);
    }
}

/// Adapter that lets the updater plugin consult the supervisor across the
/// crate boundary. The updater hands us an [`UpdateTriggerInfo`]; we look
/// up the matching routing rule (if any) so that `public_hostname` /
/// `health_path` are populated, then delegate to
/// [`DeploymentSupervisor::handle_update_trigger`] and translate its
/// outcome into a [`DispatchOutcome`] the updater understands.
///
/// On any inventory failure we degrade gracefully: return
/// `DispatchOutcome::PerformInPlace` so the updater still does *something*
/// useful instead of leaving the container stale.
pub struct SupervisorDispatcher {
    pub supervisor: Arc<DeploymentSupervisor>,
    pub inventory: Inventory,
}

#[async_trait]
impl UpdateDispatcher for SupervisorDispatcher {
    async fn dispatch(&self, info: UpdateTriggerInfo) -> DispatchOutcome {
        // The updater speaks the core HostId (= ulid::Ulid). Storage uses
        // a newtype around the same Ulid; convert before any DB call.
        let storage_host_id = HostId(info.host_id);

        let routing_rules = match self
            .inventory
            .list_routing_rules_for_host(storage_host_id)
            .await
        {
            Ok(rs) => rs,
            Err(e) => {
                // Degrade to in-place rather than leave the container stale,
                // but surface the underlying issue so it's debuggable.
                tracing::warn!(
                    host_id = %storage_host_id,
                    service = %info.service_name,
                    error = %e,
                    "supervisor dispatcher: routing-rule lookup failed, falling back to in-place"
                );
                return DispatchOutcome::PerformInPlace;
            }
        };

        // Match by service_name AND container_port (a single service can
        // expose multiple ports with different routing rules; we want the
        // rule that lines up with the port we observed).
        let rule = routing_rules.iter().find(|r| {
            r.service_name == info.service_name && info.container_port == Some(r.container_port)
        });

        let trigger = UpdateTrigger {
            container_id: info.container_id,
            host_id: storage_host_id,
            stack_id: StackId(info.stack_id),
            service_name: info.service_name,
            blue_digest: info.blue_digest,
            green_digest: info.green_digest,
            image_ref: info.image_ref,
            public_hostname: rule.map(|r| r.public_hostname.clone()),
            container_port: info.container_port,
            health_path: rule.and_then(|r| r.healthcheck_path.clone()),
            has_healthcheck: info.has_healthcheck,
            rw_volume_mounts: info.rw_volume_mounts,
            label_strategy: info.label_strategy,
        };

        match self.supervisor.handle_update_trigger(trigger).await {
            Ok(SupervisorOutcome::BlueGreenSpawned { .. }) => DispatchOutcome::Handled,
            Ok(SupervisorOutcome::AlreadyInFlight) => DispatchOutcome::Handled,
            Ok(SupervisorOutcome::InPlaceForUpdater) => DispatchOutcome::PerformInPlace,
            Err(_) => DispatchOutcome::PerformInPlace,
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
            previous_digest: None,
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
    async fn supervisor_consults_service_deploy_strategy_override() {
        use isengard_storage::service::{InsertService, ServiceState};

        let (sup, host, stack, inv) = setup().await;

        // Insert a service row with an "in-place" override stored.
        let svc_id = inv
            .insert_service(InsertService {
                host_id: host,
                stack_id: Some(stack),
                name: "web".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        inv.set_service_deploy_strategy_override(svc_id, Some("in-place"))
            .await
            .unwrap();

        // Trigger has the full BG-eligible shape (routing rule + healthcheck)
        // and NO container label — the stored "in-place" override should
        // win and route to InPlaceForUpdater anyway.
        let mut t = trigger(host, stack, true);
        t.label_strategy = None;
        let outcome = sup.handle_update_trigger(t).await.unwrap();
        assert_eq!(outcome, SupervisorOutcome::InPlaceForUpdater);
    }

    #[tokio::test]
    async fn container_label_wins_over_stored_service_override() {
        use isengard_storage::service::{InsertService, ServiceState};

        let (sup, host, stack, inv) = setup().await;

        // Service has stored "blue-green" override (would normally route to
        // BlueGreen)...
        let svc_id = inv
            .insert_service(InsertService {
                host_id: host,
                stack_id: Some(stack),
                name: "web".into(),
                image: "nginx:alpine".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        inv.set_service_deploy_strategy_override(svc_id, Some("blue-green"))
            .await
            .unwrap();

        // ...but the container label explicitly forces "in-place". Label
        // wins, so this should NOT trigger the BG driver-spawn path.
        let mut t = trigger(host, stack, true);
        t.label_strategy = Some("in-place".into());
        let outcome = sup.handle_update_trigger(t).await.unwrap();
        assert_eq!(outcome, SupervisorOutcome::InPlaceForUpdater);
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
                previous_digest: None,
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

    // ----- Phase 9F (T2): supervisor captures previous_digest -----

    /// Synthetic `PolicyLoader` returning a fixed list. Used by the T2
    /// tests to drive `resolve_failure_handling` deterministically
    /// without round-tripping through the storage DAO.
    struct FixedPolicyLoader {
        rows: Vec<isengard_core::LoadedPolicy>,
        fleet: Option<String>,
    }
    #[async_trait]
    impl isengard_core::PolicyLoader for FixedPolicyLoader {
        async fn list(
            &self,
        ) -> std::result::Result<Vec<isengard_core::LoadedPolicy>, isengard_core::PolicyLoaderError>
        {
            Ok(self.rows.clone())
        }
        async fn fleet_for(
            &self,
            _host_id: isengard_core::HostId,
        ) -> std::result::Result<Option<String>, isengard_core::PolicyLoaderError> {
            Ok(self.fleet.clone())
        }
    }

    /// Service-scope policy with `on_failure = Rollback` causes the
    /// supervisor to seed `previous_digest = Some(blue_digest)` on the
    /// deployment row. The driver task spawned alongside will fail
    /// promptly because there's no real Docker, but `previous_digest`
    /// is set on insert so it's observable BEFORE the driver runs.
    #[tokio::test]
    async fn rollback_policy_captures_previous_digest() {
        use isengard_core::policy::{FailureHandling as FH, Policy, PolicyScopeType};
        let (mut sup, host, stack, inv) = setup().await;
        let loader: Arc<dyn isengard_core::PolicyLoader> = Arc::new(FixedPolicyLoader {
            rows: vec![isengard_core::LoadedPolicy {
                scope_type: PolicyScopeType::Service,
                scope_key: "web".into(),
                body: Policy {
                    on_failure: Some(FH::Rollback),
                    ..Default::default()
                },
            }],
            fleet: Some("default".into()),
        });
        sup = sup.with_policy_loader(loader);

        let mut t = trigger(host, stack, true);
        t.blue_digest = "sha256:bluedigest".into();
        let outcome = sup.handle_update_trigger(t).await.unwrap();
        let dep_id = match outcome {
            SupervisorOutcome::BlueGreenSpawned { deployment_id } => deployment_id,
            other => panic!("expected BlueGreenSpawned, got {other:?}"),
        };

        let row = inv.get_deployment(&dep_id).await.unwrap().expect("row");
        assert_eq!(
            row.previous_digest.as_deref(),
            Some("sha256:bluedigest"),
            "Rollback policy should populate previous_digest"
        );
    }

    /// Default `Notify` policy leaves `previous_digest` NULL: the row is
    /// not eligible for automatic rollback. Same setup as above with
    /// `on_failure = Notify`.
    #[tokio::test]
    async fn notify_policy_leaves_previous_digest_null() {
        use isengard_core::policy::{FailureHandling as FH, Policy, PolicyScopeType};
        let (mut sup, host, stack, inv) = setup().await;
        let loader: Arc<dyn isengard_core::PolicyLoader> = Arc::new(FixedPolicyLoader {
            rows: vec![isengard_core::LoadedPolicy {
                scope_type: PolicyScopeType::Service,
                scope_key: "web".into(),
                body: Policy {
                    on_failure: Some(FH::Notify),
                    ..Default::default()
                },
            }],
            fleet: Some("default".into()),
        });
        sup = sup.with_policy_loader(loader);

        let mut t = trigger(host, stack, true);
        t.blue_digest = "sha256:bluedigest".into();
        let outcome = sup.handle_update_trigger(t).await.unwrap();
        let dep_id = match outcome {
            SupervisorOutcome::BlueGreenSpawned { deployment_id } => deployment_id,
            other => panic!("expected BlueGreenSpawned, got {other:?}"),
        };
        let row = inv.get_deployment(&dep_id).await.unwrap().expect("row");
        assert!(
            row.previous_digest.is_none(),
            "Notify policy must not seed previous_digest"
        );
    }
}
