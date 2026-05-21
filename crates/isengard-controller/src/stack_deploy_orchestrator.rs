#![doc = include_str!("../docs/stack-deploy-orchestrator.md")]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use isengard_core::Event;
use isengard_storage::deployment::DeploymentState;
use isengard_storage::{
    DeploymentGroup, DeploymentGroupState, HostId, InsertDeploymentGroup, Inventory, StackId,
};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::bus::EventBus;

/// Parsed parallelism setting.
///
/// Either a positive integer batch size, or "every host in one wave".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parallelism {
    /// `1`, `2`, etc. Capped at the number of target hosts in
    /// [`Parallelism::build_waves`].
    Batch(u32),
    /// `"all"`: every host in a single wave.
    All,
}

impl Parallelism {
    /// Resolves `Option<&str>` from storage to a [`Parallelism`].
    ///
    /// `None` and unrecognised strings fall back to `Batch(1)` (the
    /// rolling default).
    pub fn from_storage(raw: Option<&str>) -> Self {
        let Some(s) = raw else { return Self::Batch(1) };
        if s.eq_ignore_ascii_case("all") {
            return Self::All;
        }
        match s.parse::<u32>() {
            Ok(n) if n >= 1 => Self::Batch(n),
            _ => Self::Batch(1),
        }
    }

    /// Snapshot string for the `parallelism` column on a freshly
    /// inserted group row.
    ///
    /// Mirrors what [`Parallelism::from_storage`] accepts.
    pub fn as_storage_str(self) -> String {
        match self {
            Self::Batch(n) => n.to_string(),
            Self::All => "all".into(),
        }
    }

    /// Computes waves for a list of host ids.
    ///
    /// Hosts are sorted by their ULID string so wave membership is
    /// deterministic across runs.
    pub fn build_waves(self, hosts: &[HostId]) -> Vec<Vec<HostId>> {
        if hosts.is_empty() {
            return Vec::new();
        }
        let mut sorted = hosts.to_vec();
        sorted.sort_by_key(|h| h.0.to_string());

        let batch = match self {
            Self::All => sorted.len(),
            Self::Batch(n) => (n as usize).max(1).min(sorted.len()),
        };
        sorted.chunks(batch).map(|c| c.to_vec()).collect()
    }
}

/// Per-wave dispatch surface.
///
/// The production impl queues a `ForceUpdate` host action for every
/// host in the wave; tests record calls into a `Vec`.
#[async_trait]
pub trait WaveDispatcher: Send + Sync + 'static {
    /// Dispatches one wave.
    ///
    /// `service_name` is informational (the controller already filters
    /// per-stack updates by service); the orchestrator passes it
    /// through for trace context and for impls that want to use it.
    ///
    /// # Errors
    ///
    /// Returns `Err` on per-host queue or storage failure. The
    /// orchestrator does not retry: the caller (start path or the
    /// reconcile loop) decides what to do next.
    async fn dispatch(
        &self,
        host_ids: &[HostId],
        stack_id: StackId,
        service_name: &str,
        deployment_ids: &[String],
    ) -> Result<()>;
}

/// Production dispatcher.
///
/// Queues a stack-wide `ForceUpdate` host action for each host. The
/// agent's deployment supervisor pulls the action on its next
/// heartbeat, computes the new digest, and runs blue-green per its
/// existing strategy.
pub struct ProductionDispatcher {
    /// Shared inventory for the `queue_action` write.
    inventory: Arc<Inventory>,
}

impl ProductionDispatcher {
    /// Builds a dispatcher over the shared inventory.
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}

#[async_trait]
impl WaveDispatcher for ProductionDispatcher {
    async fn dispatch(
        &self,
        host_ids: &[HostId],
        stack_id: StackId,
        _service_name: &str,
        _deployment_ids: &[String],
    ) -> Result<()> {
        let stack = self
            .inventory
            .get_stack(stack_id)
            .await
            .context("loading stack for dispatch")?
            .ok_or_else(|| anyhow::anyhow!("stack {stack_id} not found"))?;
        for h in host_ids {
            self.inventory
                .queue_action(
                    *h,
                    isengard_storage::HostActionKind::ForceUpdate {
                        stack_name: Some(stack.name.clone()),
                    },
                )
                .await
                .with_context(|| format!("queuing ForceUpdate for host {h}"))?;
        }
        Ok(())
    }
}

/// Behaviour when a wave has at least one failed or aborted
/// deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFailure {
    /// Stop rolling forward; mark the group as `aborted`. Default.
    #[default]
    Rollback,
    /// Continue dispatching subsequent waves regardless of failures.
    Continue,
}

/// One in-memory group descriptor.
///
/// The orchestrator keeps these alive while at least one wave is
/// unfinished; on terminal transition the entry is removed.
#[derive(Debug, Clone)]
struct GroupRuntime {
    /// Pre-computed wave plan; one inner `Vec<HostId>` per wave.
    waves: Vec<Vec<HostId>>,
    /// Index of the wave currently in flight.
    current_wave: usize,
    /// Deployment ids dispatched for the current wave.
    ///
    /// Indexed by `deployment_id` for O(1) terminal-event matching.
    pending_deployment_ids: HashSet<String>,
    /// Stack the group belongs to.
    stack_id: StackId,
    /// Service name carried through to dispatch for trace context.
    service_name: String,
    /// Failure handling policy for this group.
    on_failure: OnFailure,
}

/// Outcome reported back to callers of
/// [`StackDeployOrchestrator::start_group`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Single host targeted; the orchestrator delegated straight to
    /// the existing per-host path. No group row was created.
    Direct,
    /// Multi-host group took ownership. The string is the group id.
    Group(String),
}

/// Stack-level deployment orchestrator.
#[derive(Clone)]
pub struct StackDeployOrchestrator {
    /// Shared inventory for group and deployment writes.
    inventory: Arc<Inventory>,
    /// Event bus subscription used by the background task.
    bus: Arc<EventBus>,
    /// Wave dispatcher (production or test mock).
    dispatcher: Arc<dyn WaveDispatcher>,
    /// Live group state, keyed by group id.
    state: Arc<Mutex<HashMap<String, GroupRuntime>>>,
    /// Periodic reconcile cadence. Defaults to 30 seconds in
    /// production; tests override with a shorter interval.
    reconcile_interval: Duration,
}

impl StackDeployOrchestrator {
    /// Builds an orchestrator over the shared inventory, bus, and a
    /// dispatcher.
    pub fn new(
        inventory: Arc<Inventory>,
        bus: Arc<EventBus>,
        dispatcher: Arc<dyn WaveDispatcher>,
    ) -> Self {
        Self {
            inventory,
            bus,
            dispatcher,
            state: Arc::new(Mutex::new(HashMap::new())),
            reconcile_interval: Duration::from_secs(30),
        }
    }

    /// Overrides the periodic reconciliation cadence.
    ///
    /// Tests use a short interval; production keeps the 30-second
    /// default.
    pub fn with_reconcile_interval(mut self, d: Duration) -> Self {
        self.reconcile_interval = d;
        self
    }

    /// Spins up a new group and dispatches wave 0, or bypasses
    /// entirely for single-host deploys.
    ///
    /// The caller pre-computes the deployment rows (one per host) and
    /// passes their ids; the orchestrator records the group id back
    /// onto each deployment via `set_deployment_group`.
    ///
    /// When `target_hosts.len() <= 1` the orchestrator returns
    /// [`StartOutcome::Direct`] without writing a group row, and the
    /// caller takes the existing per-host path.
    ///
    /// # Errors
    ///
    /// Returns `Err` on storage failure during the group insert, the
    /// per-deployment link, the state transition to rolling, or wave 0
    /// dispatch.
    pub async fn start_group(
        &self,
        stack_id: StackId,
        service_name: &str,
        target_hosts: Vec<HostId>,
        deployment_ids: Vec<String>,
        on_failure: OnFailure,
    ) -> Result<StartOutcome> {
        debug_assert_eq!(target_hosts.len(), deployment_ids.len());

        if target_hosts.len() <= 1 {
            return Ok(StartOutcome::Direct);
        }

        let raw_parallelism = self
            .inventory
            .get_stack_parallelism(stack_id)
            .await
            .context("loading stack parallelism")?;
        let parallelism = Parallelism::from_storage(raw_parallelism.as_deref());
        let waves = parallelism.build_waves(&target_hosts);

        let group_id = ulid::Ulid::new().to_string();
        let group = self
            .inventory
            .insert_deployment_group(InsertDeploymentGroup {
                id: group_id.clone(),
                stack_id,
                service_name: service_name.into(),
                parallelism: parallelism.as_storage_str(),
                state: DeploymentGroupState::Pending,
                target_hosts: target_hosts.clone(),
            })
            .await
            .context("inserting deployment group")?;

        // Link every per-host deployment row to the group up front. Even
        // hosts whose dispatch is deferred to wave 1+ get the link so list
        // queries return the full picture.
        for (host_idx, dep_id) in deployment_ids.iter().enumerate() {
            self.inventory
                .set_deployment_group(dep_id, &group.id)
                .await
                .with_context(|| {
                    format!(
                        "linking deployment {dep_id} (host idx {host_idx}) to group {}",
                        group.id
                    )
                })?;
        }

        // Build per-wave deployment_id slices following the host ordering used
        // by `Parallelism::build_waves`. We reorder `deployment_ids` to match
        // by zipping with target_hosts after `build_waves`'s alphabetical sort.
        let host_to_dep: HashMap<HostId, String> = target_hosts
            .iter()
            .copied()
            .zip(deployment_ids.iter().cloned())
            .collect();

        let runtime = GroupRuntime {
            waves: waves.clone(),
            current_wave: 0,
            pending_deployment_ids: HashSet::new(),
            stack_id,
            service_name: service_name.into(),
            on_failure,
        };
        self.state
            .lock()
            .await
            .insert(group.id.clone(), runtime.clone());

        // Move group to rolling and dispatch wave 0.
        self.inventory
            .update_deployment_group_state(&group.id, DeploymentGroupState::Rolling, None)
            .await?;

        if let Some(first) = waves.first() {
            let dep_ids: Vec<String> = first
                .iter()
                .map(|h| host_to_dep.get(h).cloned().unwrap_or_default())
                .collect();
            // Track which deployment ids the orchestrator is waiting on.
            {
                let mut guard = self.state.lock().await;
                if let Some(rt) = guard.get_mut(&group.id) {
                    rt.pending_deployment_ids = dep_ids.iter().cloned().collect();
                }
            }
            self.dispatcher
                .dispatch(first, stack_id, service_name, &dep_ids)
                .await
                .context("dispatching wave 0")?;
            info!(
                group_id = %group.id,
                wave = 0,
                hosts = first.len(),
                "deployment group rolling"
            );
        } else {
            // Empty wave plan: mark done immediately. Defensive only.
            self.inventory
                .update_deployment_group_state(&group.id, DeploymentGroupState::Done, None)
                .await?;
            self.state.lock().await.remove(&group.id);
        }

        Ok(StartOutcome::Group(group.id))
    }

    /// Spawns the background task that watches `deployment.*` events
    /// on the bus and advances or aborts groups accordingly.
    ///
    /// Returns a [`JoinHandle`] the caller aborts on shutdown.
    pub fn start_background(self: Arc<Self>) -> JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut rx = me.bus.subscribe();
            let mut ticker = tokio::time::interval(me.reconcile_interval);
            // First tick fires immediately: skip it; nothing to reconcile yet.
            ticker.tick().await;
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Ok(ev) => {
                            if let Err(e) = me.handle_event(&ev).await {
                                warn!(error = %e, kind = %ev.kind, "orchestrator handle_event failed");
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "orchestrator subscriber lagged; reconciling");
                            if let Err(e) = me.reconcile_all().await {
                                warn!(error = %e, "orchestrator reconcile (after lag) failed");
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = ticker.tick() => {
                        if let Err(e) = me.reconcile_all().await {
                            warn!(error = %e, "orchestrator periodic reconcile failed");
                        }
                    }
                }
            }
        })
    }

    /// Handles one event from the bus.
    ///
    /// Cheap on no-match: most events on the bus are unrelated to
    /// deployments and short-circuit on the kind prefix check.
    async fn handle_event(&self, ev: &Event) -> Result<()> {
        if !ev.kind.starts_with("deployment.") {
            return Ok(());
        }
        let Some(dep_value) = ev.metadata.get("deployment") else {
            return Ok(());
        };
        let dep: isengard_storage::deployment::Deployment =
            match serde_json::from_value(dep_value.clone()) {
                Ok(d) => d,
                Err(_) => return Ok(()),
            };
        if !dep.state.is_terminal() {
            return Ok(());
        }
        // Find the group this deployment belongs to. The schema carries
        // `group_id` on the deployments row, but the public Deployment struct
        // doesn't expose it yet, so resolve via in-memory state with a
        // storage fallback.
        let Some(group_id) = self.lookup_group_for_deployment(&dep.id).await? else {
            return Ok(()); // not part of any group
        };
        self.advance_group(&group_id, &dep).await
    }

    /// Resolves which group a deployment belongs to.
    ///
    /// Walks the in-memory state first (pending wave ids), then falls
    /// back to a storage scan of every active group's deployments.
    /// The DAO doesn't expose `deployments.group_id` on the public
    /// `Deployment` struct, so the scan is the canonical lookup path.
    async fn lookup_group_for_deployment(&self, deployment_id: &str) -> Result<Option<String>> {
        // The DAO doesn't expose this column on the public Deployment struct,
        // so reach in with a raw query. The pool method is `pub(crate)`,
        // because we live in another crate, drive this through
        // `list_deployments_by_group` lookups instead.
        //
        // Faster: the orchestrator already tracks pending deployment ids per
        // group in memory. Walk in-memory state first.
        let guard = self.state.lock().await;
        for (gid, rt) in guard.iter() {
            if rt.pending_deployment_ids.contains(deployment_id) {
                return Ok(Some(gid.clone()));
            }
            // Maybe the deployment is from an earlier wave we already
            // advanced past. Walk the full wave plan to confirm membership.
            for wave in &rt.waves {
                for _h in wave {
                    // We don't have host->deployment_id mapping in runtime
                    // beyond pending; fall through to storage scan if needed.
                    // (Storage scan handled below.)
                }
            }
            let _ = gid;
        }
        drop(guard);
        // Storage fallback: scan each known group's deployments. Cheap because
        // active groups are few.
        let active = self
            .inventory
            .list_active_deployment_groups()
            .await
            .unwrap_or_default();
        for g in active {
            let deps = self.inventory.list_deployments_by_group(&g.id).await?;
            if deps.iter().any(|d| d.id == deployment_id) {
                return Ok(Some(g.id));
            }
        }
        Ok(None)
    }

    /// Treats one terminal deployment as a hint to re-evaluate the
    /// group's current wave.
    ///
    /// Re-queries storage for the current wave's deployments so the
    /// decision is grounded in the canonical state, not the in-memory
    /// snapshot which can lag a missed event.
    async fn advance_group(
        &self,
        group_id: &str,
        _trigger: &isengard_storage::deployment::Deployment,
    ) -> Result<()> {
        let mut guard = self.state.lock().await;
        let Some(rt) = guard.get_mut(group_id) else {
            return Ok(());
        };
        let runtime = rt.clone();
        drop(guard);

        let deps = self
            .inventory
            .list_deployments_by_group(group_id)
            .await
            .context("listing deployments by group")?;

        let current_wave_hosts: HashSet<HostId> = runtime
            .waves
            .get(runtime.current_wave)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let current_wave_deps: Vec<_> = deps
            .iter()
            .filter(|d| current_wave_hosts.contains(&d.host_id))
            .collect();
        let all_terminal = !current_wave_deps.is_empty()
            && current_wave_deps.iter().all(|d| d.state.is_terminal());
        if !all_terminal {
            return Ok(());
        }
        let any_failed = current_wave_deps
            .iter()
            .any(|d| matches!(d.state, DeploymentState::Failed | DeploymentState::Aborted));
        if any_failed && runtime.on_failure == OnFailure::Rollback {
            let err_msg = current_wave_deps
                .iter()
                .find(|d| d.state == DeploymentState::Failed || d.state == DeploymentState::Aborted)
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| "wave failed".into());
            self.inventory
                .update_deployment_group_state(
                    group_id,
                    DeploymentGroupState::Aborted,
                    Some(&err_msg),
                )
                .await?;
            self.state.lock().await.remove(group_id);
            info!(group_id = %group_id, "deployment group aborted on wave failure");
            return Ok(());
        }
        let next_wave = runtime.current_wave + 1;
        if next_wave >= runtime.waves.len() {
            // All waves complete. Decide between Done and Failed (continue mode).
            let any_failure_overall = deps
                .iter()
                .any(|d| matches!(d.state, DeploymentState::Failed | DeploymentState::Aborted));
            let new_state = if any_failure_overall && runtime.on_failure == OnFailure::Continue {
                DeploymentGroupState::Failed
            } else {
                DeploymentGroupState::Done
            };
            self.inventory
                .update_deployment_group_state(group_id, new_state, None)
                .await?;
            self.state.lock().await.remove(group_id);
            info!(group_id = %group_id, state = new_state.as_str(), "deployment group terminal");
            return Ok(());
        }
        // Dispatch the next wave.
        let next_hosts = runtime.waves[next_wave].clone();
        let next_dep_ids: Vec<String> = deps
            .iter()
            .filter(|d| next_hosts.contains(&d.host_id))
            .map(|d| d.id.clone())
            .collect();
        {
            let mut g = self.state.lock().await;
            if let Some(rt) = g.get_mut(group_id) {
                rt.current_wave = next_wave;
                rt.pending_deployment_ids = next_dep_ids.iter().cloned().collect();
            }
        }
        self.dispatcher
            .dispatch(
                &next_hosts,
                runtime.stack_id,
                &runtime.service_name,
                &next_dep_ids,
            )
            .await
            .with_context(|| format!("dispatching wave {next_wave}"))?;
        info!(
            group_id = %group_id,
            wave = next_wave,
            hosts = next_hosts.len(),
            "deployment group advanced to next wave"
        );
        Ok(())
    }

    /// Reconciles every active group.
    ///
    /// Queries storage for each group's current wave and advances when
    /// every deployment is terminal. Used as a periodic safety net and
    /// after a broadcast `Lag`.
    async fn reconcile_all(&self) -> Result<()> {
        let group_ids: Vec<String> = self.state.lock().await.keys().cloned().collect();
        for gid in group_ids {
            // Build a synthetic trigger by picking the most recently updated
            // terminal deployment in the group, if any.
            let deps = self
                .inventory
                .list_deployments_by_group(&gid)
                .await
                .unwrap_or_default();
            if let Some(d) = deps.into_iter().rev().find(|d| d.state.is_terminal()) {
                self.advance_group(&gid, &d).await?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Storage helper missing on `Inventory` (tiny enough to inline here).
// ---------------------------------------------------------------------------

/// Internal extension trait for the bits of "list active groups" the
/// DAO doesn't expose directly.
#[async_trait]
trait ListActiveGroups {
    /// Lists every non-terminal deployment group across every stack.
    async fn list_active_deployment_groups(&self) -> Result<Vec<DeploymentGroup>>;
}

#[async_trait]
impl ListActiveGroups for Inventory {
    async fn list_active_deployment_groups(&self) -> Result<Vec<DeploymentGroup>> {
        // Active = pending OR rolling. We don't have a pre-built helper; reuse
        // `list_deployment_groups` per stack would require knowing every stack
        // up front. Use a direct query via the public DAO surface by reading
        // every group state and filtering. Inventory doesn't expose a "list
        // all groups" method yet; iterate over hosts -> stacks -> groups.
        let stacks = self.list_stacks(None).await.context("list_stacks")?;
        let mut out = Vec::new();
        for s in stacks {
            let groups = self
                .list_deployment_groups(s.id, 50)
                .await
                .context("list_deployment_groups")?;
            for g in groups {
                if !matches!(
                    g.state,
                    DeploymentGroupState::Done
                        | DeploymentGroupState::Failed
                        | DeploymentGroupState::Aborted
                ) {
                    out.push(g);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::deployment::{DeployStrategy, InsertDeployment};
    use isengard_storage::host::EnrollHost;
    use isengard_storage::stack::{InsertStack, StackSource};
    use ulid::Ulid;

    fn host_id_from_byte(b: u8) -> HostId {
        let bytes = [b; 16];
        HostId::from_bytes(bytes)
    }

    #[test]
    fn parallelism_from_storage_defaults_to_one() {
        assert_eq!(Parallelism::from_storage(None), Parallelism::Batch(1));
        assert_eq!(Parallelism::from_storage(Some("")), Parallelism::Batch(1));
        assert_eq!(
            Parallelism::from_storage(Some("garbage")),
            Parallelism::Batch(1)
        );
    }

    #[test]
    fn parallelism_from_storage_parses_int_and_all() {
        assert_eq!(Parallelism::from_storage(Some("1")), Parallelism::Batch(1));
        assert_eq!(Parallelism::from_storage(Some("3")), Parallelism::Batch(3));
        assert_eq!(Parallelism::from_storage(Some("ALL")), Parallelism::All);
        assert_eq!(Parallelism::from_storage(Some("all")), Parallelism::All);
    }

    #[test]
    fn build_waves_batch_one_chunks_into_singles() {
        let h = vec![
            host_id_from_byte(1),
            host_id_from_byte(2),
            host_id_from_byte(3),
        ];
        let waves = Parallelism::Batch(1).build_waves(&h);
        assert_eq!(waves.len(), 3);
        for w in &waves {
            assert_eq!(w.len(), 1);
        }
    }

    #[test]
    fn build_waves_all_returns_one_wave() {
        let h = vec![
            host_id_from_byte(1),
            host_id_from_byte(2),
            host_id_from_byte(3),
        ];
        let waves = Parallelism::All.build_waves(&h);
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 3);
    }

    #[test]
    fn build_waves_batch_two_makes_pairs() {
        let h = vec![
            host_id_from_byte(1),
            host_id_from_byte(2),
            host_id_from_byte(3),
            host_id_from_byte(4),
        ];
        let waves = Parallelism::Batch(2).build_waves(&h);
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].len(), 2);
        assert_eq!(waves[1].len(), 2);
    }

    /// Recording dispatcher used by tests. Captures every wave it would have
    /// dispatched without touching the real routing pusher.
    struct RecordingDispatcher {
        calls: Mutex<Vec<Vec<HostId>>>,
    }
    impl RecordingDispatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl WaveDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            host_ids: &[HostId],
            _stack_id: StackId,
            _service_name: &str,
            _deployment_ids: &[String],
        ) -> Result<()> {
            self.calls.lock().await.push(host_ids.to_vec());
            Ok(())
        }
    }

    async fn fresh_inv() -> Inventory {
        Inventory::open_in_memory().await.unwrap()
    }

    async fn enroll(inv: &Inventory, name: &str) -> HostId {
        inv.enroll_host(EnrollHost {
            fingerprint: format!("fp-{name}"),
            hostname: name.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
        })
        .await
        .unwrap()
    }

    async fn make_stack(inv: &Inventory, host: HostId) -> StackId {
        inv.insert_stack(InsertStack {
            host_id: host,
            name: "blog".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap()
    }

    fn dep(host: HostId, stack: StackId) -> InsertDeployment {
        InsertDeployment {
            id: Ulid::new().to_string(),
            host_id: host,
            stack_id: stack,
            service_name: "web".into(),
            strategy: DeployStrategy::BlueGreen,
            state: DeploymentState::Pending,
            blue_container: None,
            green_container: None,
            blue_digest: "sha256:aaa".into(),
            green_digest: "sha256:bbb".into(),
            public_hostname: None,
            health_path: None,
            container_port: None,
            metadata_json: None,
            previous_digest: None,
        }
    }

    #[tokio::test]
    async fn single_host_returns_direct_outcome() {
        let inv = Arc::new(fresh_inv().await);
        let h1 = enroll(&inv, "h1").await;
        let stack = make_stack(&inv, h1).await;

        let bus = Arc::new(EventBus::new());
        let disp = RecordingDispatcher::new();
        let orch = StackDeployOrchestrator::new(inv.clone(), bus, disp.clone());

        let d = inv.insert_deployment(dep(h1, stack)).await.unwrap();
        let outcome = orch
            .start_group(stack, "web", vec![h1], vec![d.id], OnFailure::Rollback)
            .await
            .unwrap();
        assert_eq!(outcome, StartOutcome::Direct);
        assert!(disp.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn multi_host_batch_one_dispatches_only_first_wave() {
        let inv = Arc::new(fresh_inv().await);
        let h1 = enroll(&inv, "h1").await;
        let h2 = enroll(&inv, "h2").await;
        let h3 = enroll(&inv, "h3").await;
        let stack = make_stack(&inv, h1).await;
        // Default parallelism = 1.

        let bus = Arc::new(EventBus::new());
        let disp = RecordingDispatcher::new();
        let orch = StackDeployOrchestrator::new(inv.clone(), bus, disp.clone());

        let mut dep_ids = Vec::new();
        for h in [h1, h2, h3] {
            let d = inv.insert_deployment(dep(h, stack)).await.unwrap();
            dep_ids.push(d.id);
        }
        let outcome = orch
            .start_group(stack, "web", vec![h1, h2, h3], dep_ids, OnFailure::Rollback)
            .await
            .unwrap();
        assert!(matches!(outcome, StartOutcome::Group(_)));
        let calls = disp.calls.lock().await;
        assert_eq!(calls.len(), 1, "only wave 0 dispatched");
        assert_eq!(calls[0].len(), 1, "batch=1 means 1 host per wave");
    }
}
