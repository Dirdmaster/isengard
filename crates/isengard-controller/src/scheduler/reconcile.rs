//! Reconcile loop body. Phase 0.14 step 6.
//!
//! The scheduler is the **planner**: it computes which host should run
//! which replica of a service and persists that decision to the
//! `placements` table. Emitting `placement.*` events on transitions
//! gives the operator (and a future dashboard tile) a real-time view.
//!
//! Scope cut for 0.14: the scheduler does NOT directly dispatch
//! HostActions / WriteCompose. The existing per-host compose apply
//! path remains the actuator. The placements table is the integration
//! seam: a future commit (0.14.1 or 0.15) can teach the apply path to
//! consult `placements` for "which host should I be on" rather than the
//! current host-pinned semantics. Emitted `placement.*` events give
//! the operator immediate feedback.
//!
//! Decision #3 (operator-locked 2026-05-11): when `where:` matches zero
//! hosts, the placement stays Pending; auto-place fires when a host
//! enrolls or a heartbeat brings a newly-eligible label set. The trigger
//! lives in [`Scheduler::on_host_enroll`] and
//! [`Scheduler::on_heartbeat_labels`] which call into
//! [`Scheduler::reconcile_all`].

use chrono::Utc;
use isengard_core::Event;
use isengard_core::placement::Placement;
use isengard_storage::host::HostId;
use isengard_storage::service::ServiceId;
use isengard_storage::{PlacementState, UpsertPlacement};
use tracing::{debug, info, warn};

#[cfg(test)]
use super::HostHealth;
use super::PlacementAssignment;
use super::eligible::eligible_hosts;
use super::events::kind;
use super::{Scheduler, assign};

impl Scheduler {
    /// Reconcile a single service. Reads the placement intent from
    /// `placement_source`, computes the desired set against the in-memory
    /// eligible-hosts snapshot, persists changes, emits events.
    pub(super) async fn reconcile_service_inner(
        &self,
        service_id: ServiceId,
    ) -> anyhow::Result<()> {
        let Some(placement) = self.placement_source.placement_for(service_id) else {
            debug!(
                service = %service_id,
                "scheduler: no placement intent (legacy singleton), skipping reconcile",
            );
            return Ok(());
        };

        let hosts = self.inventory.list_hosts().await?;
        let (labels_by_host, health_by_host) = {
            let state = self.state.lock().await;
            (state.labels.clone(), state.health.clone())
        };

        let selector = placement_selector(&placement);
        let eligible = eligible_hosts(&hosts, &labels_by_host, &health_by_host, selector);

        let current_rows = self
            .inventory
            .list_placements_by_service(service_id)
            .await?;
        let current: Vec<PlacementAssignment> =
            current_rows.iter().cloned().map(Into::into).collect();

        // Compute the desired (host, replica) want-set per the verb.
        let want: Vec<(HostId, u32)> = match &placement {
            Placement::Singleton { .. } => assign::assign_singleton(&eligible, &current),
            Placement::Spread { count, .. } => {
                assign::assign_spread(*count, &eligible, &current, None)
            }
            Placement::Global { .. } => assign::assign_global(&eligible),
            Placement::On { host, .. } => {
                let resolved = hosts.iter().find(|h| &h.hostname == host);
                match resolved {
                    Some(h) => assign::assign_on(h.id, &eligible),
                    None => {
                        // Host not enrolled: pending, emit unknown_host.
                        self.emit_placement_event(
                            kind::UNKNOWN_HOST,
                            Some(service_id),
                            None,
                            serde_json::json!({"host": host}),
                        );
                        Vec::new()
                    }
                }
            }
        };

        // Diff.
        let to_create: Vec<(HostId, u32)> = want
            .iter()
            .copied()
            .filter(|w| {
                !current
                    .iter()
                    .any(|c| c.host_id == w.0 && c.replica_index == w.1)
            })
            .collect();
        let to_drain: Vec<PlacementAssignment> = current
            .iter()
            .filter(|c| {
                !want
                    .iter()
                    .any(|w| w.0 == c.host_id && w.1 == c.replica_index)
                    && c.state != PlacementState::Draining
            })
            .cloned()
            .collect();

        // Degradation hint: spread couldn't fill the requested count.
        if let Placement::Spread { count, .. } = &placement
            && (want.len() as u32) < *count
        {
            self.emit_placement_event(
                kind::DEGRADED,
                Some(service_id),
                None,
                serde_json::json!({"wanted": count, "got": want.len()}),
            );
        }

        // No-eligible-hosts event (skip for the On variant: it emitted
        // unknown_host above when the hostname wasn't enrolled, OR
        // host_gone is more accurate if the pinned host disconnected).
        if want.is_empty() && !matches!(&placement, Placement::On { .. }) {
            // Drain stale rows; the service stays Pending until auto-place.
            for cur in &current {
                if cur.state != PlacementState::Draining {
                    self.persist_state(
                        service_id,
                        cur.host_id,
                        cur.replica_index,
                        PlacementState::Draining,
                        Some(serde_json::json!({
                            "reason": "no eligible hosts; draining",
                        })),
                    )
                    .await?;
                }
            }
            self.emit_placement_event(
                kind::NO_ELIGIBLE_HOSTS,
                Some(service_id),
                None,
                serde_json::json!({
                    "selector": selector.map(|s| s.to_string()),
                    "host_count": hosts.len(),
                }),
            );
        }

        // For pinned-host placements where the host disappeared
        // post-place: emit host_gone (in addition to unknown_host above
        // when the host never existed).
        if let Placement::On { host, .. } = &placement
            && want.is_empty()
            && hosts.iter().any(|h| &h.hostname == host)
        {
            self.emit_placement_event(
                kind::HOST_GONE,
                Some(service_id),
                None,
                serde_json::json!({"host": host}),
            );
        }

        // Persist creates.
        for (host_id, replica_index) in to_create {
            self.persist_state(
                service_id,
                host_id,
                replica_index,
                PlacementState::Active,
                Some(serde_json::json!({
                    "kind": "placement.created",
                    "ts": Utc::now().to_rfc3339(),
                })),
            )
            .await?;
            self.emit_placement_event(
                kind::CREATED,
                Some(service_id),
                Some(host_id),
                serde_json::json!({"replica_index": replica_index}),
            );
        }

        // Persist drains.
        for cur in to_drain {
            self.persist_state(
                service_id,
                cur.host_id,
                cur.replica_index,
                PlacementState::Draining,
                Some(serde_json::json!({
                    "kind": "placement.removed",
                    "ts": Utc::now().to_rfc3339(),
                })),
            )
            .await?;
            self.emit_placement_event(
                kind::REMOVED,
                Some(service_id),
                Some(cur.host_id),
                serde_json::json!({"replica_index": cur.replica_index}),
            );
        }

        // Refresh the in-memory cache so the next reconcile sees the
        // just-written state.
        let refreshed = self
            .inventory
            .list_placements_by_service(service_id)
            .await?
            .into_iter()
            .map(PlacementAssignment::from)
            .collect::<Vec<_>>();
        {
            let mut state = self.state.lock().await;
            if refreshed.is_empty() {
                state.placements.remove(&service_id);
            } else {
                state.placements.insert(service_id, refreshed);
            }
        }
        Ok(())
    }

    /// Walk every service with a known placement intent. For 0.14 we
    /// enumerate every service in the inventory; the placement source
    /// itself returns `None` for services that don't have a verb, so
    /// reconcile_service_inner short-circuits.
    pub(super) async fn reconcile_all_inner(&self) -> anyhow::Result<()> {
        let services = self.inventory.list_services(None).await?;
        for svc in services {
            if let Err(e) = self.reconcile_service_inner(svc.id).await {
                warn!(service = %svc.id, error = %e, "reconcile_service failed");
            }
        }
        Ok(())
    }

    async fn persist_state(
        &self,
        service_id: ServiceId,
        host_id: HostId,
        replica_index: u32,
        state: PlacementState,
        last_event: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let evt_str = last_event.map(|v| v.to_string());
        self.inventory
            .upsert_placement(UpsertPlacement {
                service_id,
                host_id,
                replica_index,
                state,
                last_event: evt_str,
            })
            .await
            .map_err(|e| anyhow::anyhow!("upsert_placement: {e}"))?;
        Ok(())
    }

    pub(super) fn emit_placement_event(
        &self,
        kind: &str,
        service_id: Option<ServiceId>,
        host_id: Option<HostId>,
        metadata: serde_json::Value,
    ) {
        let summary = match (service_id, host_id) {
            (Some(s), Some(h)) => format!("{kind} service={s} host={h}"),
            (Some(s), None) => format!("{kind} service={s}"),
            (None, Some(h)) => format!("{kind} host={h}"),
            (None, None) => kind.to_string(),
        };
        // Attach service_id to metadata for downstream filtering.
        let mut metadata = metadata;
        if let Some(obj) = metadata.as_object_mut() {
            if let Some(s) = service_id {
                obj.insert("service_id".into(), serde_json::json!(s.0));
            }
        }
        let event = Event {
            kind: kind.to_string(),
            occurred_at: Utc::now(),
            summary,
            host_id: host_id.map(|h| h.0),
            metadata,
            ..Default::default()
        };
        self.bus.publish(event);
    }

    /// Public entry point: reconcile a single service.
    pub async fn reconcile_service_full(&self, service_id: ServiceId) -> anyhow::Result<()> {
        self.reconcile_service_inner(service_id).await
    }

    /// Public entry point: reconcile every service.
    pub async fn reconcile_all_full(&self) -> anyhow::Result<()> {
        self.reconcile_all_inner().await
    }

    /// Auto-place trigger (operator decision #3): a host enrolled or
    /// a heartbeat brought new labels. Re-evaluate every service so
    /// previously-Pending services with `where:` selectors get placed
    /// onto the newly-eligible host.
    ///
    /// Wired into the heartbeat / enroll paths in step 7; visible here
    /// so the test harness can call it directly.
    #[allow(dead_code)] // wired up in step 7
    pub(crate) async fn auto_place_after_event(
        &self,
        reason: &str,
        host_id: HostId,
    ) -> anyhow::Result<()> {
        info!(
            host = %host_id,
            reason = %reason,
            "scheduler: re-evaluating pending placements (auto-place hook)",
        );
        self.reconcile_all_inner().await
    }
}

fn placement_selector(p: &Placement) -> Option<&isengard_core::placement::LabelSelector> {
    match p {
        Placement::Singleton { selector } => selector.as_ref(),
        Placement::Spread { selector, .. } => selector.as_ref(),
        Placement::Global { selector } => selector.as_ref(),
        Placement::On { selector, .. } => selector.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::scheduler::PlacementSource;
    use isengard_core::placement::LabelSelector;
    use isengard_storage::host::EnrollHost;
    use isengard_storage::service::ServiceState;
    use isengard_storage::stack::{InsertStack, StackSource};
    use isengard_storage::{Inventory, service::InsertService};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::time::Duration;

    struct FixedSource(HashMap<ServiceId, Placement>);

    impl PlacementSource for FixedSource {
        fn placement_for(&self, service_id: ServiceId) -> Option<Placement> {
            self.0.get(&service_id).cloned()
        }
    }

    async fn add_host(inv: &Inventory, name: &str) -> HostId {
        inv.enroll_host(EnrollHost {
            hostname: name.into(),
            fingerprint: format!("fp-{name}"),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.14".into(),
            docker_version: "24.0".into(),
        })
        .await
        .unwrap()
    }

    async fn add_service(inv: &Inventory, host_id: HostId, name: &str) -> ServiceId {
        let stack_id = inv
            .insert_stack(InsertStack {
                host_id,
                name: format!("{name}-stack"),
                source: StackSource::Compose,
            })
            .await
            .unwrap();
        inv.insert_service(InsertService {
            host_id,
            stack_id: Some(stack_id),
            name: name.into(),
            image: "nginx".into(),
            state: ServiceState::Running,
        })
        .await
        .unwrap()
    }

    fn label_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    async fn fresh_with(
        placements: HashMap<ServiceId, Placement>,
    ) -> (Arc<Inventory>, Arc<Scheduler>) {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(placements));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        (inv, sched)
    }

    async fn mark_all_healthy(sched: &Scheduler, inv: &Inventory) {
        let mut state = sched.state.lock().await;
        for h in inv.list_hosts().await.unwrap() {
            state.health.insert(h.id, HostHealth::Healthy);
        }
    }

    #[tokio::test]
    async fn reconcile_spread_three_hosts_creates_three_placements() {
        let (inv, sched) = fresh_with(HashMap::new()).await;
        let a = add_host(&inv, "alice").await;
        let _b = add_host(&inv, "bob").await;
        let _c = add_host(&inv, "carol").await;
        let svc = add_service(&inv, a, "web").await;
        // Rewire the scheduler with the now-known svc id.
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::Spread {
                count: 3,
                selector: None,
            },
        );
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let bus = Arc::new(EventBus::new());
        let sched2 = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched2, &inv).await;
        let _ = sched;
        sched2.reconcile_service_full(svc).await.unwrap();
        let rows = inv.list_placements_by_service(svc).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn reconcile_spread_two_hosts_three_replicas_round_robins() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let a = add_host(&inv, "alice").await;
        let _b = add_host(&inv, "bob").await;
        let svc = add_service(&inv, a, "web").await;
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::Spread {
                count: 3,
                selector: None,
            },
        );
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let rows = inv.list_placements_by_service(svc).await.unwrap();
        assert_eq!(rows.len(), 3, "3 replicas round-robin onto 2 hosts");
    }

    #[tokio::test]
    async fn reconcile_singleton_no_eligible_emits_no_eligible_hosts() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let svc = add_service(&inv, alice, "web").await;
        let selector = LabelSelector::parse("role==worker").unwrap();
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::Singleton {
                selector: Some(selector),
            },
        );
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.kind, "placement.no_eligible_hosts");
    }

    #[tokio::test]
    async fn auto_place_after_label_change_places_pending_service() {
        // Operator decision #3 (locked 2026-05-11): when a label
        // change makes a previously-Pending service eligible, the
        // scheduler auto-places it.
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let svc = add_service(&inv, alice, "web").await;
        let selector = LabelSelector::parse("role==worker").unwrap();
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::Singleton {
                selector: Some(selector),
            },
        );
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        assert_eq!(
            inv.list_placements_by_service(svc).await.unwrap().len(),
            0,
            "no placement created before label match",
        );

        sched
            .on_heartbeat_labels(alice, label_map(&[("role", "worker")]))
            .await;
        sched
            .auto_place_after_event("heartbeat_labels", alice)
            .await
            .unwrap();

        let rows = inv.list_placements_by_service(svc).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host_id, alice);
        assert_eq!(rows[0].state, PlacementState::Active);
    }

    #[tokio::test]
    async fn reconcile_no_placement_intent_is_noop() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let svc = add_service(&inv, alice, "web").await;
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(HashMap::new()));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        sched.reconcile_service_full(svc).await.unwrap();
        assert!(
            inv.list_placements_by_service(svc)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reconcile_global_places_on_every_eligible_host() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let _bob = add_host(&inv, "bob").await;
        let _carol = add_host(&inv, "carol").await;
        let svc = add_service(&inv, alice, "node-exporter").await;
        let mut m = HashMap::new();
        m.insert(svc, Placement::Global { selector: None });
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        assert_eq!(inv.list_placements_by_service(svc).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn reconcile_on_known_host_places_singleton() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let svc = add_service(&inv, alice, "db").await;
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::On {
                host: "alice".into(),
                selector: None,
            },
        );
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let rows = inv.list_placements_by_service(svc).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host_id, alice);
    }

    #[tokio::test]
    async fn reconcile_on_unknown_host_emits_unknown_host() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let svc = add_service(&inv, alice, "db").await;
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::On {
                host: "missing".into(),
                selector: None,
            },
        );
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.kind, "placement.unknown_host");
        assert!(
            inv.list_placements_by_service(svc)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reconcile_spread_after_disconnect_drains_lost_host() {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        let alice = add_host(&inv, "alice").await;
        let _bob = add_host(&inv, "bob").await;
        let svc = add_service(&inv, alice, "web").await;
        let mut m = HashMap::new();
        m.insert(
            svc,
            Placement::Spread {
                count: 2,
                selector: None,
            },
        );
        let bus = Arc::new(EventBus::new());
        let src: Arc<dyn PlacementSource> = Arc::new(FixedSource(m));
        let sched = Arc::new(
            Scheduler::new(inv.clone(), bus, Duration::from_secs(60), src)
                .await
                .unwrap(),
        );
        mark_all_healthy(&sched, &inv).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let rows = inv.list_placements_by_service(svc).await.unwrap();
        assert_eq!(rows.len(), 2);

        // Bob disconnects: scheduler marks his health Disconnected and
        // reconciles. Bob's row gets drained, alice keeps both
        // (round-robin onto the remaining host).
        let bob_id = inv
            .list_hosts()
            .await
            .unwrap()
            .into_iter()
            .find(|h| h.hostname == "bob")
            .unwrap()
            .id;
        sched.on_host_disconnect_long(bob_id).await;
        sched.reconcile_service_full(svc).await.unwrap();
        let rows = inv.list_placements_by_service(svc).await.unwrap();
        // The bob row stays as Draining; alice gets both new wants.
        let draining = rows
            .iter()
            .filter(|r| r.state == PlacementState::Draining)
            .count();
        let active = rows
            .iter()
            .filter(|r| r.state == PlacementState::Active)
            .count();
        assert!(draining >= 1, "bob's row should be Draining, rows={rows:?}");
        assert!(active >= 1, "alice should still have active placement");
    }
}
