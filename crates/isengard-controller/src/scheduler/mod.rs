//! Phase 0.14 placement scheduler.
//!
//! The scheduler resolves each service's `Placement` (parsed from the
//! compose source) into a concrete set of `(host_id, replica_index)`
//! assignments, persists them to the `placements` table, and drives the
//! existing per-host stack apply path with the result.
//!
//! Layout:
//! - [`Scheduler`] (this file): public struct + lifecycle + trigger
//!   methods.
//! - [`desired`]: compute the "want" set from a `Placement`.
//! - [`eligible`]: host eligibility under a `LabelSelector`.
//! - [`assign`]: spread / global / on / singleton math.
//! - [`reconcile`]: reconcile loop (compare want vs current, emit ops).
//! - [`events`]: `placement.*` event helpers.
//!
//! Owned by `Controller::start()`. Reads `Inventory` and publishes via
//! `EventBus`; writes placements + dispatches host actions via the
//! existing `HostAction` queue (added in step 6).

pub mod assign;
pub mod desired;
pub mod eligible;
pub mod events;
pub mod reconcile;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use isengard_core::placement::Placement;
use isengard_storage::host::HostId;
use isengard_storage::service::ServiceId;
use isengard_storage::{Inventory, PlacementRow};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::bus::EventBus;

/// Default fleet-wide grace period before a disconnected host's
/// placements are reassigned. Configurable via the controller flag
/// `--placement-grace-secs`. Operator decision 2026-05-11 deferred
/// per-service grace to 0.15+.
pub const DEFAULT_GRACE_SECS: u64 = 60;

/// Default reconcile timer interval. The scheduler runs event-driven
/// AND timer-driven; the timer is a safety net against missed events.
pub const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// Trait the scheduler uses to look up a service's parsed [`Placement`].
/// The compose source of truth lives outside the controller's storage
/// crate (parsing happens in `isengard-agent::compose_reconciler`); for
/// phase 0.14 the controller-side caller supplies an implementation
/// when constructing the scheduler. Step 7 wires the compose broker.
pub trait PlacementSource: Send + Sync {
    /// Return the parsed placement for a service. `Ok(None)` means
    /// "service has no placement verb; treat as default singleton on
    /// its existing host" (the migration backfill seeded those rows
    /// already, so the scheduler is a noop for them).
    fn placement_for(&self, service_id: ServiceId) -> Option<Placement>;
}

/// Skeleton placement source that always returns `None`. Useful for
/// the step-3 wire-in pass and tests that don't yet model compose.
#[derive(Debug, Default, Clone)]
pub struct EmptyPlacementSource;

impl PlacementSource for EmptyPlacementSource {
    fn placement_for(&self, _service_id: ServiceId) -> Option<Placement> {
        None
    }
}

/// One assignment in the scheduler's in-memory state. The DB-backed
/// `PlacementRow` is the authority; this is the projection the
/// reconcile loop walks every tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementAssignment {
    pub service_id: ServiceId,
    pub host_id: HostId,
    pub replica_index: u32,
    pub assigned_at: DateTime<Utc>,
    pub state: isengard_storage::PlacementState,
}

impl From<PlacementRow> for PlacementAssignment {
    fn from(r: PlacementRow) -> Self {
        Self {
            service_id: r.service_id,
            host_id: r.host_id,
            replica_index: r.replica_index,
            assigned_at: r.assigned_at,
            state: r.state,
        }
    }
}

/// Health gate for a host. Derived from `last_seen_at` + the disconnect
/// grace; updated by `on_heartbeat_labels` / `on_host_disconnect_long`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostHealth {
    Healthy,
    Disconnected {
        since: DateTime<Utc>,
    },
    Flapping {
        since: DateTime<Utc>,
        flip_count: u32,
    },
}

/// In-memory scheduler state. Rebuilt from `placements` + `agent_labels`
/// + `hosts` rows on controller boot.
#[derive(Debug, Default)]
pub struct SchedulerState {
    pub placements: HashMap<ServiceId, Vec<PlacementAssignment>>,
    pub labels: HashMap<HostId, BTreeMap<String, String>>,
    pub health: HashMap<HostId, HostHealth>,
    /// Hash of the last-seen labels per host. Used by
    /// `on_heartbeat_labels` to skip a reconcile when the heartbeat
    /// label set hasn't changed since the previous tick (prevents
    /// reconcile chatter under noisy agents).
    pub label_hash: HashMap<HostId, u64>,
}

/// Phase 0.14 placement scheduler.
///
/// Step 3 skeleton: fields land here so the public API stays stable as
/// step 4-6 fill in eligibility, assignment, and reconcile. Each field
/// is consumed by a later step (see the comment under each).
#[allow(dead_code)] // wires up in step 6
pub struct Scheduler {
    /// Inventory handle for read (placements / labels / hosts / services)
    /// and write (placement upserts). Consumed by `reconcile_*` (step 6).
    pub(crate) inventory: Arc<Inventory>,
    /// Event bus for emitting `placement.*` events. Consumed by step 6.
    pub(crate) bus: Arc<EventBus>,
    /// Fleet-wide disconnect grace. Consumed by `on_host_disconnect_long`
    /// + the reconcile path (step 6).
    pub(crate) grace: Duration,
    /// Timer interval for `reconcile_all`. Configurable; defaults to 15s.
    pub(crate) reconcile_interval: Duration,
    /// In-memory state (placements / labels / health). Mutated by every
    /// trigger entry point.
    pub(crate) state: Arc<Mutex<SchedulerState>>,
    /// Looks up a service's parsed `Placement`. Wired to the compose
    /// broker in step 7.
    pub(crate) placement_source: Arc<dyn PlacementSource>,
}

impl Scheduler {
    /// Build a new scheduler. Rebuilds in-memory state from the
    /// `placements` and `agent_labels` tables, so post-restart the first
    /// reconcile tick has a complete picture.
    pub async fn new(
        inventory: Arc<Inventory>,
        bus: Arc<EventBus>,
        grace: Duration,
        placement_source: Arc<dyn PlacementSource>,
    ) -> anyhow::Result<Self> {
        let mut state = SchedulerState::default();

        // Load every persisted placement into the per-service vec map.
        let rows = inventory
            .list_all_placements()
            .await
            .map_err(|e| anyhow::anyhow!("loading placements at scheduler boot: {e}"))?;
        for row in rows {
            state
                .placements
                .entry(row.service_id)
                .or_default()
                .push(PlacementAssignment::from(row));
        }

        // Load every host's last-known label set. Labels for hosts
        // without a row stay empty; that matches "older agent" semantics
        // (no labels, no `where:` match).
        let hosts = inventory
            .list_hosts()
            .await
            .map_err(|e| anyhow::anyhow!("loading hosts at scheduler boot: {e}"))?;
        for host in &hosts {
            let labels = inventory
                .list_agent_labels(host.id)
                .await
                .map_err(|e| anyhow::anyhow!("loading agent_labels for {}: {e}", host.id))?;
            state.label_hash.insert(host.id, label_hash(&labels));
            state.labels.insert(host.id, labels);
            // Health is computed lazily; on boot we treat every host as
            // healthy and let the first disconnect_long / heartbeat
            // delta correct.
            state.health.insert(host.id, HostHealth::Healthy);
        }

        Ok(Self {
            inventory,
            bus,
            grace,
            reconcile_interval: DEFAULT_RECONCILE_INTERVAL,
            state: Arc::new(Mutex::new(state)),
            placement_source,
        })
    }

    /// Spawn the reconcile timer task. Returns a handle the caller
    /// stores; aborting on shutdown stops the loop.
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        let interval = self.reconcile_interval;
        let me = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the first immediate tick; the first reconcile happens
            // one interval after boot. Heartbeat / enroll / disconnect
            // event triggers carry the in-flight reconciles between
            // ticks.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = me.reconcile_all().await {
                    warn!(error = %e, "scheduler: reconcile_all failed");
                }
            }
        })
    }

    // === trigger surface ====================================================
    //
    // These are the entry points the rest of the controller calls into.
    // Step 3 wires them as stubs; step 6 makes them do real work.

    /// Reconcile a single service. Stubbed until step 6.
    pub async fn reconcile_service(&self, service_id: ServiceId) -> anyhow::Result<()> {
        debug!(service = %service_id, "scheduler: reconcile_service (stub)");
        Ok(())
    }

    /// Walk every service and reconcile. Stubbed until step 6.
    pub async fn reconcile_all(&self) -> anyhow::Result<()> {
        debug!("scheduler: reconcile_all (stub)");
        Ok(())
    }

    /// Heartbeat label update. Cached label set is replaced; reconcile
    /// is triggered only when the new set differs from the previous
    /// (hash-compare to skip noisy agents). Stubbed until step 6.
    pub async fn on_heartbeat_labels(&self, host_id: HostId, new_labels: BTreeMap<String, String>) {
        let new_hash = label_hash(&new_labels);
        let changed = {
            let mut state = self.state.lock().await;
            let prev = state.label_hash.insert(host_id, new_hash);
            state.labels.insert(host_id, new_labels);
            prev != Some(new_hash)
        };
        if changed {
            debug!(host = %host_id, "scheduler: labels changed (no reconcile yet, step 6)");
        }
    }

    /// Host enrolled. Stubbed until step 6.
    pub async fn on_host_enroll(&self, host_id: HostId) {
        let mut state = self.state.lock().await;
        state.health.insert(host_id, HostHealth::Healthy);
        info!(host = %host_id, "scheduler: host enrolled (stub)");
    }

    /// Host disconnected past the long-grace threshold. Stubbed until
    /// step 6.
    pub async fn on_host_disconnect_long(&self, host_id: HostId) {
        let mut state = self.state.lock().await;
        state
            .health
            .insert(host_id, HostHealth::Disconnected { since: Utc::now() });
        info!(host = %host_id, "scheduler: host disconnect_long (stub)");
    }

    // === test seams ==========================================================

    #[cfg(test)]
    pub(crate) async fn state_snapshot(&self) -> SchedulerStateSnapshot {
        let state = self.state.lock().await;
        SchedulerStateSnapshot {
            placement_count: state.placements.values().map(|v| v.len()).sum(),
            label_host_count: state.labels.len(),
            healthy_host_count: state
                .health
                .values()
                .filter(|h| matches!(h, HostHealth::Healthy))
                .count(),
        }
    }
}

/// Quick check the test harness uses to assert scheduler boot rebuild.
#[cfg(test)]
#[derive(Debug)]
pub struct SchedulerStateSnapshot {
    pub placement_count: usize,
    pub label_host_count: usize,
    pub healthy_host_count: usize,
}

/// Hash the label set so `on_heartbeat_labels` can skip a reconcile
/// when nothing changed. BTreeMap ordering is deterministic; a simple
/// FNV-1a over the key/value bytes is enough.
pub fn label_hash(labels: &BTreeMap<String, String>) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for (k, v) in labels {
        for b in k.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= u64::from(b'=');
        h = h.wrapping_mul(0x100000001b3);
        for b in v.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= u64::from(b',');
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::host::EnrollHost;
    use isengard_storage::service::{InsertService, ServiceState};
    use isengard_storage::stack::{InsertStack, StackSource};

    async fn fresh_inventory() -> Arc<Inventory> {
        Arc::new(Inventory::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn new_scheduler_with_empty_state() {
        let inv = fresh_inventory().await;
        let bus = Arc::new(EventBus::new());
        let sched = Scheduler::new(
            inv,
            bus,
            Duration::from_secs(60),
            Arc::new(EmptyPlacementSource),
        )
        .await
        .unwrap();
        let snap = sched.state_snapshot().await;
        assert_eq!(snap.placement_count, 0);
        assert_eq!(snap.label_host_count, 0);
        assert_eq!(snap.healthy_host_count, 0);
    }

    #[tokio::test]
    async fn new_scheduler_rebuilds_placements_from_disk() {
        let inv = fresh_inventory().await;
        let host_id = inv
            .enroll_host(EnrollHost {
                hostname: "alice".into(),
                fingerprint: "fp-alice".into(),
                fleet: "default".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                agent_version: "0.14".into(),
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
        let svc_id = inv
            .insert_service(InsertService {
                host_id,
                stack_id: Some(stack_id),
                name: "web".into(),
                image: "nginx".into(),
                state: ServiceState::Running,
            })
            .await
            .unwrap();
        inv.upsert_placement(isengard_storage::UpsertPlacement {
            service_id: svc_id,
            host_id,
            replica_index: 0,
            state: isengard_storage::PlacementState::Active,
            last_event: None,
        })
        .await
        .unwrap();

        let bus = Arc::new(EventBus::new());
        let sched = Scheduler::new(
            inv,
            bus,
            Duration::from_secs(60),
            Arc::new(EmptyPlacementSource),
        )
        .await
        .unwrap();
        let snap = sched.state_snapshot().await;
        assert_eq!(snap.placement_count, 1);
        assert_eq!(snap.label_host_count, 1);
        assert_eq!(snap.healthy_host_count, 1);
    }

    #[tokio::test]
    async fn label_hash_stable_for_same_map() {
        let mut a = BTreeMap::new();
        a.insert("role".to_string(), "worker".to_string());
        a.insert("tier".to_string(), "gpu".to_string());
        let mut b = BTreeMap::new();
        b.insert("tier".to_string(), "gpu".to_string());
        b.insert("role".to_string(), "worker".to_string());
        assert_eq!(label_hash(&a), label_hash(&b));
    }

    #[tokio::test]
    async fn label_hash_differs_for_different_value() {
        let mut a = BTreeMap::new();
        a.insert("role".to_string(), "worker".to_string());
        let mut b = BTreeMap::new();
        b.insert("role".to_string(), "control".to_string());
        assert_ne!(label_hash(&a), label_hash(&b));
    }

    #[tokio::test]
    async fn on_heartbeat_labels_records_new_set() {
        let inv = fresh_inventory().await;
        let bus = Arc::new(EventBus::new());
        let sched = Scheduler::new(
            inv,
            bus,
            Duration::from_secs(60),
            Arc::new(EmptyPlacementSource),
        )
        .await
        .unwrap();
        let host_id = HostId::from_bytes([42u8; 16]);
        let mut labels = BTreeMap::new();
        labels.insert("role".to_string(), "worker".to_string());
        sched.on_heartbeat_labels(host_id, labels.clone()).await;
        let state = sched.state.lock().await;
        assert_eq!(state.labels.get(&host_id), Some(&labels));
    }
}
