//! Disconnect monitor: polls the inventory and emits
//! `agent.disconnect_long` for hosts unreachable past a threshold.
//!
//! An in-memory `HashSet` debounces repeat emissions; entries clear
//! when a host comes back inside the threshold. In production the
//! threshold is 4 hours with a 60-second poll cadence.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_storage::{Host, HostId, Inventory, Journal};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::bus::EventBus;
use crate::persist_and_broadcast;

/// Background worker that emits `agent.disconnect_long` for hosts
/// past the stale threshold.
pub struct DisconnectMonitor {
    /// Shared inventory; the monitor reads the host list every tick.
    inventory: Arc<Inventory>,
    /// Shared journal; receives the persisted event before the bus
    /// publish.
    journal: Arc<Journal>,
    /// Shared event bus; downstream subscribers (notifier, scheduler)
    /// react here.
    bus: Arc<EventBus>,
    /// "Stale" cutoff: how long since `last_seen_at` before the
    /// monitor emits.
    threshold: chrono::Duration,
    /// Cadence at which the monitor re-evaluates the host list.
    poll_interval: Duration,
    /// Optional placement scheduler.
    ///
    /// When set, the monitor calls `on_host_disconnect_long` after
    /// every emission so the scheduler can re-route placements off the
    /// stale host.
    scheduler: Option<Arc<crate::scheduler::Scheduler>>,
}

impl DisconnectMonitor {
    /// Builds a monitor with the given threshold and poll cadence.
    pub fn new(
        inventory: Arc<Inventory>,
        journal: Arc<Journal>,
        bus: Arc<EventBus>,
        threshold_secs: i64,
        poll_interval_secs: f64,
    ) -> Self {
        Self {
            inventory,
            journal,
            bus,
            threshold: chrono::Duration::seconds(threshold_secs),
            poll_interval: Duration::from_secs_f64(poll_interval_secs),
            scheduler: None,
        }
    }

    /// Attaches a placement scheduler.
    ///
    /// Every `agent.disconnect_long` emission then also fires the
    /// scheduler's auto-reroute path.
    pub fn with_scheduler(mut self, scheduler: Arc<crate::scheduler::Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Spawns the polling task.
    ///
    /// Returns a [`JoinHandle`] the caller aborts on shutdown.
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        let already_emitted: Arc<Mutex<HashSet<HostId>>> = Arc::new(Mutex::new(HashSet::new()));
        let me = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(me.poll_interval);
            // First tick fires immediately — skip it to avoid emit-on-startup
            // for hosts that look stale because the controller just booted.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = me.tick(&already_emitted).await {
                    warn!(error = %e, "disconnect_monitor tick failed");
                }
            }
        })
    }

    /// One pass over every host. Emits or recovers based on
    /// [`should_emit`].
    async fn tick(&self, already_emitted: &Arc<Mutex<HashSet<HostId>>>) -> anyhow::Result<()> {
        let hosts = self.inventory.list_hosts().await?;
        let now = Utc::now();
        let mut set = already_emitted.lock().await;

        for h in hosts {
            let last_seen = effective_last_seen(&h);
            let stale = should_emit(now, last_seen, &set, &h.id, self.threshold);
            if stale == EmitDecision::Emit {
                info!(
                    host_id = %h.id,
                    fingerprint = %h.fingerprint,
                    "agent.disconnect_long: host unreachable past threshold"
                );
                let event = Event {
                    kind: "agent.disconnect_long".into(),
                    occurred_at: now,
                    host_id: Some(h.id.into()),
                    summary: format!(
                        "agent {} unreachable for over {}s",
                        h.fingerprint,
                        self.threshold.num_seconds()
                    ),
                    ..Default::default()
                };
                persist_and_broadcast(&self.journal, &self.bus, event).await;
                if let Some(sched) = self.scheduler.as_ref() {
                    sched.on_host_disconnect_long(h.id).await;
                }
                set.insert(h.id);
            } else if stale == EmitDecision::Recovered {
                debug!(host_id = %h.id, "host returned within threshold; clearing emit-once flag");
                set.remove(&h.id);
            }
        }
        Ok(())
    }
}

/// Maps a host's `last_seen_at` (Unix seconds; `None` when it never
/// heartbeat) onto a comparable `DateTime<Utc>`.
///
/// Falls back to `enrolled_at` for hosts that have never reported in,
/// so a never-heartbeat host enrolled past the threshold counts as
/// stale.
fn effective_last_seen(h: &Host) -> DateTime<Utc> {
    let secs = h.last_seen_at.unwrap_or(h.enrolled_at);
    DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

/// Outcome of one [`should_emit`] decision.
#[derive(Debug, PartialEq, Eq)]
enum EmitDecision {
    /// Stale and not yet emitted: send the event.
    Emit,
    /// Was previously emitted but now within threshold: clear from the
    /// emit-once set.
    Recovered,
    /// No action.
    Idle,
}

/// Pure function for the emit decision so the policy is unit-testable.
fn should_emit(
    now: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    already_emitted: &HashSet<HostId>,
    host: &HostId,
    threshold: chrono::Duration,
) -> EmitDecision {
    let stale = now - last_seen > threshold;
    let was_emitted = already_emitted.contains(host);
    match (stale, was_emitted) {
        (true, false) => EmitDecision::Emit,
        (false, true) => EmitDecision::Recovered,
        _ => EmitDecision::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostId {
        HostId::from(ulid::Ulid::new())
    }

    #[test]
    fn fresh_host_not_emitted_is_idle() {
        let now = Utc::now();
        let last_seen = now;
        let set = HashSet::new();
        assert_eq!(
            should_emit(now, last_seen, &set, &host(), chrono::Duration::seconds(60)),
            EmitDecision::Idle
        );
    }

    #[test]
    fn stale_host_not_emitted_emits() {
        let now = Utc::now();
        let last_seen = now - chrono::Duration::seconds(120);
        let set = HashSet::new();
        let h = host();
        assert_eq!(
            should_emit(now, last_seen, &set, &h, chrono::Duration::seconds(60)),
            EmitDecision::Emit
        );
    }

    #[test]
    fn stale_host_already_emitted_is_idle() {
        let now = Utc::now();
        let last_seen = now - chrono::Duration::seconds(120);
        let mut set = HashSet::new();
        let h = host();
        set.insert(h);
        assert_eq!(
            should_emit(now, last_seen, &set, &h, chrono::Duration::seconds(60)),
            EmitDecision::Idle
        );
    }

    #[test]
    fn fresh_host_previously_emitted_is_recovered() {
        let now = Utc::now();
        let last_seen = now;
        let mut set = HashSet::new();
        let h = host();
        set.insert(h);
        assert_eq!(
            should_emit(now, last_seen, &set, &h, chrono::Duration::seconds(60)),
            EmitDecision::Recovered
        );
    }
}
