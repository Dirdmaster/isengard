# Phase 4f: agent.disconnect_long Background Monitor + Phase 4 e2e

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** The controller produces `agent.disconnect_long` events for hosts unreachable beyond a threshold (default 4h). The events are journaled AND broadcast on the EventBus, so the notifier can forward them to Telegram/Discord/HTTP. After 4f, Phase 4 is complete: events flow agent→controller→journal→bus→notifier→Telegram, AND controller-internal events (disconnect_long) take the same path.

**Architecture:** A new `disconnect_monitor.rs` module in `isengard-controller` spawns a tokio task on `run_controller` startup. The task ticks every `poll_interval_secs` (default 60s, configurable for tests). Each tick: list hosts via `Inventory::list_hosts`, compute `now - last_seen_at`, emit `agent.disconnect_long` for any host past `threshold_secs` (default 14400 = 4h) that we haven't already emitted for. An in-memory `HashSet<HostId>` tracks already-emitted; clears entries when a host comes back inside the threshold. Emission goes through a new `publish_internal(event)` helper that journals THEN publishes to bus — same ordering as agent-emitted events.

**Tech stack:** No new workspace deps. Uses existing `chrono`, `tokio::time::interval`, `tokio::sync::broadcast`.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §7.

---

## Scope

**In:**
- `disconnect_monitor.rs` module: `DisconnectMonitor::new(inventory, journal, bus, threshold_secs, poll_interval_secs)` + `start()` returning `JoinHandle<()>`.
- Threshold + poll-interval are constructor parameters — `run_controller` passes 14400 + 60 in production, tests pass 1 + 0.2 (or similar).
- Persisting the event: a small helper `persist_and_broadcast(journal, bus, event)` lives in `lib.rs` (or `bus.rs`) — extracted from the duplicated journal+bus pattern in the Sync handler so both producers share it.
- `ControllerOptions` stays as-is (disconnect_monitor uses constants in production); we don't expose threshold via CLI in 4f. v1.x can if real ops need it.
- Integration test: `crates/isengard-controller/tests/disconnect_monitor_e2e.rs` builds Inventory + Journal + EventBus, manually inserts a host with `last_seen_at` 2 seconds ago, starts the monitor with `threshold=1s, poll=200ms`, subscribes to the bus, asserts an `agent.disconnect_long` event arrives within 2s.
- Wire `DisconnectMonitor::start()` into `run_controller` AFTER `Server::serve` is set up but before the await (so the monitor is running while the server runs); abort the handle on shutdown.
- Unit tests for: HashSet "don't re-emit if already emitted" + "clear when host returns" semantics. Pure function `should_emit(now, host_last_seen, already_emitted) -> bool` extracted for testability.

**Out (deferred to v1.x):**
- Configurable thresholds via CLI/config (currently a constructor param)
- `agent.reconnected` event (could fire when an already-emitted host comes back; v1.x)
- Persistent already-emitted state across controller restarts (in-memory only; on restart we may re-emit once per host that's still down — acceptable for v1)
- Per-host threshold overrides (e.g., "this host is allowed to be offline 24h")

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes — baseline 142 + new tests
3. `just ci-local` clean (cargo-deny mandatory)
4. Tag `v0.1.0-alpha.phase4f` set locally + tag `v0.1.0-alpha.phase4-complete` for the milestone
5. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-controller/
├── src/
│   ├── lib.rs                          # MODIFY: spawn DisconnectMonitor; extract persist_and_broadcast helper
│   ├── service.rs                      # MODIFY: use persist_and_broadcast
│   ├── disconnect_monitor.rs           # NEW
│   └── (others unchanged)
└── tests/
    └── disconnect_monitor_e2e.rs       # NEW
```

---

## Task 1: persist_and_broadcast helper extracted

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs` (new helper)
- Modify: `crates/isengard-controller/src/service.rs` (use helper)

- [ ] **Step 1: Add the helper to lib.rs**

In `crates/isengard-controller/src/lib.rs`, add a new public function (near the top, after the `pub mod` declarations):

```rust
use std::sync::Arc;

use isengard_core::Event;
use isengard_storage::{InsertEvent, Journal};

use crate::bus::EventBus;

/// Journal an event then broadcast it on the bus. Used by both the Sync
/// handler (for agent-originated events) and controller-internal producers
/// like `disconnect_monitor`.
///
/// On journal write failure, broadcasts NO event — better to drop than to
/// notify on something we have no record of.
pub async fn persist_and_broadcast(
    journal: &Journal,
    bus: &EventBus,
    event: Event,
) {
    let insert = InsertEvent {
        host_id: event.host_id.map(|id| id.into()),
        kind: event.kind.clone(),
        container_name: event.container_name.clone(),
        image: event.image.clone(),
        old_digest: event.old_digest.clone(),
        new_digest: event.new_digest.clone(),
        error: event.error.clone(),
        summary: event.summary.clone(),
        metadata_json: if event.metadata.is_null() {
            None
        } else {
            Some(event.metadata.to_string())
        },
        occurred_at: event.occurred_at,
    };
    if let Err(e) = journal.insert(insert).await {
        tracing::warn!(error = %e, kind = %event.kind, "journal.insert failed; dropping event");
        return;
    }
    bus.publish(event);
}

// keep Arc usage in scope
#[allow(dead_code)]
const _ARC_USAGE: fn() = || {
    let _: Arc<EventBus>;
};
```

(Drop the `_ARC_USAGE` constant if `Arc` is already used elsewhere in lib.rs.)

- [ ] **Step 2: Update Sync handler in service.rs to call the helper**

Find the existing inline journal + broadcast code in the Sync handler (added in 4b-T4). It currently builds an InsertEvent inline and calls `journal.insert(...).await` then `bus.publish(...)`. Replace that block with a single call:

```rust
                    crate::persist_and_broadcast(&self.journal, &self.bus, core_ev).await;
```

(`self.journal` and `self.bus` are already on the service struct from 4b-T4.)

Note: the Sync handler currently mutates `core_ev.host_id = Some(host_id.into());` BEFORE the persist+broadcast. Keep that mutation in place — pass the mutated event to the helper. The host_id assignment is the controller's responsibility (the event arrives without one).

- [ ] **Step 3: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. Existing 13 controller tests still pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/lib.rs crates/isengard-controller/src/service.rs
cd ~/Projects/isengard && git commit -m "refactor(controller): extract persist_and_broadcast helper for shared event flow"
```

**Self-review checklist:**
- [ ] All controller tests pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: DisconnectMonitor module

**Files:**
- Create: `crates/isengard-controller/src/disconnect_monitor.rs`
- Modify: `crates/isengard-controller/src/lib.rs` (`pub mod disconnect_monitor;`)

- [ ] **Step 1: Create disconnect_monitor.rs**

Create `crates/isengard-controller/src/disconnect_monitor.rs`:

```rust
//! Polls the inventory periodically; emits `agent.disconnect_long` for hosts
//! unreachable beyond a threshold. Uses an in-memory HashSet to debounce
//! repeat emissions; entries are cleared when a host comes back inside the
//! threshold.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use isengard_core::Event;
use isengard_storage::{HostId, Inventory, Journal};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::bus::EventBus;
use crate::persist_and_broadcast;

pub struct DisconnectMonitor {
    inventory: Arc<Inventory>,
    journal: Arc<Journal>,
    bus: Arc<EventBus>,
    threshold: chrono::Duration,
    poll_interval: Duration,
}

impl DisconnectMonitor {
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
        }
    }

    /// Spawn the polling task. Returns a JoinHandle the caller should abort
    /// on shutdown.
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

    async fn tick(&self, already_emitted: &Arc<Mutex<HashSet<HostId>>>) -> anyhow::Result<()> {
        let hosts = self.inventory.list_hosts().await?;
        let now = Utc::now();
        let mut set = already_emitted.lock().await;

        for h in hosts {
            let stale = should_emit(now, h.last_seen_at, &set, &h.id, self.threshold);
            if stale == EmitDecision::Emit {
                info!(
                    host_id = %h.id,
                    fingerprint = %h.fingerprint,
                    "agent.disconnect_long: host unreachable past threshold"
                );
                let event = Event {
                    kind: "agent.disconnect_long".into(),
                    occurred_at: now,
                    host_id: Some(h.id.clone().into()),
                    summary: format!(
                        "agent {} unreachable for over {}s",
                        h.fingerprint,
                        self.threshold.num_seconds()
                    ),
                    ..Default::default()
                };
                persist_and_broadcast(&self.journal, &self.bus, event).await;
                set.insert(h.id);
            } else if stale == EmitDecision::Recovered {
                debug!(host_id = %h.id, "host returned within threshold; clearing emit-once flag");
                set.remove(&h.id);
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum EmitDecision {
    /// Stale + not yet emitted: send the event.
    Emit,
    /// Was previously emitted, but now within threshold: clear from set.
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
        set.insert(h.clone());
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
        set.insert(h.clone());
        assert_eq!(
            should_emit(now, last_seen, &set, &h, chrono::Duration::seconds(60)),
            EmitDecision::Recovered
        );
    }
}
```

- [ ] **Step 2: Add `pub mod disconnect_monitor;` to lib.rs**

In `crates/isengard-controller/src/lib.rs`:

```rust
pub mod disconnect_monitor;
```

- [ ] **Step 3: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-controller disconnect_monitor:: 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: 4 unit tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/disconnect_monitor.rs crates/isengard-controller/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(controller): DisconnectMonitor + should_emit policy (4 unit tests)"
```

---

## Task 3: Wire DisconnectMonitor into run_controller

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs` (run_controller)

- [ ] **Step 1: Spawn the monitor + abort on shutdown**

In `run_controller`, after the existing block that builds `Arc<Journal>` + `Arc<EventBus>` and starts plugins, but BEFORE `Server::serve(...).await`, add:

```rust
    // Background task: detect long-disconnected agents and emit
    // `agent.disconnect_long` (4h threshold, 60s poll cadence in production).
    let disconnect_monitor = std::sync::Arc::new(disconnect_monitor::DisconnectMonitor::new(
        inventory.clone(),
        journal.clone(),
        bus.clone(),
        14400,  // 4 hours
        60.0,   // 60s poll
    ));
    let disconnect_handle = disconnect_monitor.start();
```

After `Server::serve(...)` returns (whatever the existing shutdown path looks like — ctrl_c await, or the handler returns from a select!), abort the handle:

```rust
    disconnect_handle.abort();
```

If the existing shutdown sequence uses a tokio::select! over multiple things, add the monitor's handle to the cleanup path right before plugin_host::stop_controller_plugins.

- [ ] **Step 2: Build + tests + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. All existing tests pass.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(controller): run_controller spawns DisconnectMonitor (4h threshold, 60s poll)"
```

---

## Task 4: Integration test — disconnect_monitor emits within seconds

**Files:**
- Create: `crates/isengard-controller/tests/disconnect_monitor_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-controller/tests/disconnect_monitor_e2e.rs`:

```rust
//! Integration test: enroll a host with a stale `last_seen_at`, start the
//! monitor with a tiny threshold + fast polling, assert the agent.disconnect_long
//! event reaches the bus within 2 seconds.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use isengard_controller::bus::EventBus;
use isengard_controller::disconnect_monitor::DisconnectMonitor;
use isengard_storage::{EnrollHost, Inventory, Journal};
use tempfile::tempdir;

#[tokio::test]
async fn disconnect_monitor_emits_for_stale_host() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("isengard.db");

    // Inventory + Journal share the SQLite file.
    let inventory = Arc::new(Inventory::open(&db).await.unwrap());
    let journal = Arc::new(Journal::open(&db).await.unwrap());
    let bus = Arc::new(EventBus::new());

    // Enroll a host so it exists in the inventory.
    let enroll = EnrollHost {
        fingerprint: "test-fp-stale".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        hostname: "stale-host".to_string(),
        version: "test".to_string(),
    };
    let host_id = inventory.enroll_host(enroll).await.unwrap();

    // Touch with a stale timestamp by going through inventory's update path.
    // Inventory::touch_host uses "now" — to simulate stale, we wait + use a
    // tight threshold instead.
    // Easier: threshold = 0 seconds means EVERY host is stale.
    // Actually we want a 1s threshold and to wait 1.5s.

    // Subscribe to bus before starting monitor.
    let mut rx = bus.subscribe();

    let monitor = Arc::new(DisconnectMonitor::new(
        inventory.clone(),
        journal.clone(),
        bus.clone(),
        1,    // 1-second threshold
        0.2,  // 200ms poll
    ));
    let _handle = monitor.start();

    // Wait for the host to age past 1s threshold + at least one full poll cycle.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Drain bus with a 2s timeout.
    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    let event = received.expect("timed out waiting for event").expect("recv error");
    assert_eq!(event.kind, "agent.disconnect_long");
    assert_eq!(event.host_id.as_ref().map(|id| ulid::Ulid::from(*id)), Some(host_id.into()));
    assert!(event.summary.contains("stale-host") || event.summary.contains("test-fp-stale"));

    // Verify it landed in the journal too.
    let rows = journal.list_recent(10).await.unwrap();
    assert!(rows.iter().any(|r| r.kind == "agent.disconnect_long"));
}
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller --test disconnect_monitor_e2e 2>&1 | tail -15
```

Expected: passes within ~2 seconds.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/tests/disconnect_monitor_e2e.rs
cd ~/Projects/isengard && git commit -m "test(controller): disconnect_monitor emits agent.disconnect_long for stale host"
```

---

## Task 5: CI gate + tags (4f + phase4-complete)

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4f`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 142 baseline + 4 unit + 1 integration = 147+. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4f -m "phase 4f: agent.disconnect_long monitor + persist_and_broadcast helper"
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4-complete -m "phase 4 complete: event journal + notifier (telegram/discord/http) + disconnect monitoring"
cd ~/Projects/isengard && git tag -l | grep -E "phase4"
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 142 baseline + new tests, zero failures
- [ ] `just ci-local` clean (cargo-deny mandatory)
- [ ] Tags `v0.1.0-alpha.phase4f` + `v0.1.0-alpha.phase4-complete` exist locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (sub §7) | Plan task |
|---|---|
| Background task in controller | Tasks 2 + 3 |
| 4h threshold (configurable) | Task 2 (constructor param, default 14400 in run_controller) |
| 60s polling cadence | Task 3 (`60.0` arg) |
| In-memory `HashSet` for already-emitted debounce | Task 2 (already_emitted) |
| Clear when host returns | Task 2 (EmitDecision::Recovered branch) |
| Pure function for testability | Task 2 (`should_emit`) |
| Journal + broadcast same path as agent events | Task 1 (`persist_and_broadcast`), Task 2 uses it |
| Integration test of full flow | Task 4 |

Phase 4 closes after 4f. Combined with 4a-4e: agents emit events → controller journals + broadcasts → notifier consumes from bus → channels (Telegram/Discord/HTTP) deliver. Controller-internal events (disconnect_long) take same path.

**Type consistency check:**
- `DisconnectMonitor::new(inventory, journal, bus, threshold_secs: i64, poll_interval_secs: f64)` matches the integration test call.
- `should_emit(now, last_seen, &HashSet<HostId>, &HostId, chrono::Duration) -> EmitDecision` — pure, takes refs.
- `persist_and_broadcast(&Journal, &EventBus, Event)` — async, no Result; errors logged + dropped per spec invariant ("better drop than notify on something we have no record of").
- `host_id: HostId` from storage — converted via the `From` impls added in 4a-T4 to `ulid::Ulid` for the core `Event.host_id` field.

**No new workspace deps.**

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4f-disconnect-monitor.md`. Subagent-driven execution.
