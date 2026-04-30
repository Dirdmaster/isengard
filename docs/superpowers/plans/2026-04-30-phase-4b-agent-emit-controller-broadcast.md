# Phase 4b: Agent EventEmitter + Controller Persist & Broadcast

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Wire the Phase 4a plumbing end-to-end. Agent's plugins receive a real `EventEmitter` that multiplexes events onto the existing Sync stream. Controller's Sync handler matches the new `AgentMessage::Event` variant, persists each event to the `Journal`, and broadcasts to the `EventBus`. After 4b, an event emitted by any agent plugin lands in the controller's SQLite + every active subscriber gets it.

**Architecture:** A new `OutboundEmitter` on the agent side: holds a `tokio::sync::mpsc::Sender<Event>` whose receiver is owned by the sync loop. `run_sync_loop` is extended to read from the mpsc alongside the heartbeat ticker via `tokio::select!`, sending each event as `AgentMessage { payload: Some(Payload::Event(...)) }`. Controller-side: `run_controller` constructs `Arc<Journal>` (sharing the SQLite file with the inventory) and `Arc<EventBus>`, both passed into `ControllerService`. The Sync handler, on each inbound frame, matches against the new payload variant; if Event, decode → InsertEvent → `journal.insert(...)` → `bus.publish(...)`.

**Tech stack:** No new workspace deps. Reuses tokio mpsc + broadcast.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §2-§6.

---

## Scope

**In:**
- `OutboundEmitter` type in `crates/isengard-agent/src/sync.rs` (or a new `events.rs` if cleaner). Implements `EventEmitter`. Constructed with the mpsc Sender; its `emit` call sends on the mpsc (drops + WARN if full — backpressure-tolerant).
- `run_sync_loop` extension: takes a `mpsc::Receiver<Event>` alongside its other inputs, includes it in the `select!`. On each event, send as a new outbound `AgentMessage`.
- Wire the emitter into `run_agent`: build the mpsc, wrap the Sender in `OutboundEmitter`, attach it to `PluginContext` via `with_events`, pass the Receiver into the sync loop.
- Convert `Event` to/from the proto `Event` message — pure functions in `crates/isengard-agent/src/events_wire.rs` (or inline if small) and `crates/isengard-controller/src/events_wire.rs`. Both crates need this; a small duplication is fine for v1, or define it in `isengard-proto` as a `From` impl. Choose: define `From<isengard_core::Event> for crate::v1::Event` and reverse in `isengard-proto/src/lib.rs` as conversion impls. That avoids duplication.
- `ControllerService` gains `journal: Arc<Journal>` + `bus: Arc<EventBus>` fields. `run_controller` constructs them.
- Sync handler: match on the new payload variant, look up the host_id (the connection's already-resolved host from the SyncHello), build `InsertEvent`, persist, broadcast. On persist error, log WARN but don't break the stream.
- Integration test (`crates/isengard-agent/tests/events_e2e.rs`): spin up controller, agent enrolls, agent emits a synthetic event via the emitter, assert the journal contains it within 2s. Uses the existing in-process test harness pattern from `enroll_e2e.rs`.

**Out (deferred):**
- Updater emitting actual update events (4c)
- Any subscriber consuming from EventBus (4d)
- agent.disconnect_long (4f)
- Replay queue if agent disconnects mid-emit (v1.x — currently events emitted while disconnected are dropped)
- Persistent outbound queue (v1.x)
- Backpressure feedback to plugins (v1.x — current model: emitter drops + WARN if mpsc is full)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 112 baseline + new tests
3. `just ci-local` clean
4. New integration test passes (or skips if no Docker — but it doesn't need Docker; it's pure in-process bollard-free)
5. Tag `v0.1.0-alpha.phase4b` set locally
6. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-proto/
└── src/lib.rs                              # MODIFY: + From impls between core::Event and v1::Event

crates/isengard-agent/
├── src/
│   ├── lib.rs                              # MODIFY: build mpsc, attach OutboundEmitter, pass Receiver to sync loop
│   ├── sync.rs                             # MODIFY: select! over mpsc, send AgentMessage::Event
│   └── events.rs                           # NEW: OutboundEmitter (impl EventEmitter)
└── tests/
    └── events_e2e.rs                       # NEW: agent emits, journal contains it

crates/isengard-controller/
├── src/
│   ├── lib.rs                              # MODIFY: run_controller builds Journal + EventBus, passes to ControllerService
│   ├── service.rs                          # MODIFY: ControllerService fields, Sync handler matches Event variant
│   └── ...
```

---

## Task 1: Proto-to-core Event conversion

**Files:**
- Modify: `crates/isengard-proto/src/lib.rs`

- [ ] **Step 1: Add From impls**

In `crates/isengard-proto/src/lib.rs`, append (after the existing module + descriptor declarations):

```rust
// --- conversions between proto Event and core Event ---

#[cfg(feature = "core-conversion")]
mod conversions {
    use chrono::{DateTime, Utc};
    use isengard_core::Event as CoreEvent;
    use crate::v1::Event as ProtoEvent;

    impl From<CoreEvent> for ProtoEvent {
        fn from(e: CoreEvent) -> Self {
            ProtoEvent {
                kind: e.kind,
                occurred_at: e.occurred_at.to_rfc3339(),
                summary: e.summary,
                container_name: e.container_name,
                image: e.image,
                old_digest: e.old_digest,
                new_digest: e.new_digest,
                error: e.error,
                metadata_json: if e.metadata.is_null() {
                    None
                } else {
                    Some(e.metadata.to_string())
                },
            }
        }
    }

    impl TryFrom<ProtoEvent> for CoreEvent {
        type Error = String;

        fn try_from(p: ProtoEvent) -> Result<Self, String> {
            let occurred_at = DateTime::parse_from_rfc3339(&p.occurred_at)
                .map_err(|e| format!("bad occurred_at: {e}"))?
                .with_timezone(&Utc);
            let metadata = match p.metadata_json {
                Some(s) if !s.is_empty() => {
                    serde_json::from_str(&s).map_err(|e| format!("bad metadata_json: {e}"))?
                }
                _ => serde_json::Value::Null,
            };
            Ok(CoreEvent {
                kind: p.kind,
                occurred_at,
                host_id: None, // Set by the controller from the connection context.
                summary: p.summary,
                container_name: p.container_name,
                image: p.image,
                old_digest: p.old_digest,
                new_digest: p.new_digest,
                error: p.error,
                metadata,
            })
        }
    }
}
```

Add the feature in `crates/isengard-proto/Cargo.toml`:

```toml
[features]
default = ["core-conversion"]
core-conversion = ["dep:isengard-core", "dep:chrono", "dep:serde_json"]

[dependencies]
# existing deps...
isengard-core = { workspace = true, optional = true }
chrono = { workspace = true, optional = true }
serde_json = { workspace = true, optional = true }
```

(If `isengard-proto` already depends on chrono/serde_json/core unconditionally, remove the `optional = true` and the feature; just put the conversions module behind a config-gate or unconditional import. The feature gate is defensive — proto crates shouldn't always pull core, but for our workspace it's fine.)

**Simpler alternative**: drop the feature gate entirely. Just import unconditionally — we own all the crates and want the conversions everywhere. Use this simpler form:

```rust
// In crates/isengard-proto/src/lib.rs, append unconditionally:
use chrono::{DateTime, Utc};
use isengard_core::Event as CoreEvent;
use crate::v1::Event as ProtoEvent;

impl From<CoreEvent> for ProtoEvent {
    fn from(e: CoreEvent) -> Self {
        // ... same body
    }
}

impl TryFrom<ProtoEvent> for CoreEvent {
    type Error = String;
    fn try_from(p: ProtoEvent) -> Result<Self, String> {
        // ... same body
    }
}
```

And in `crates/isengard-proto/Cargo.toml` `[dependencies]`:

```toml
isengard-core.workspace = true
chrono.workspace = true
serde_json.workspace = true
```

Use this simpler form unless circular-dep issues bite (they shouldn't — core doesn't depend on proto).

- [ ] **Step 2: Build + test**

```bash
cd ~/Projects/isengard && cargo build -p isengard-proto 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-proto 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-proto/src/lib.rs crates/isengard-proto/Cargo.toml Cargo.lock
cd ~/Projects/isengard && git commit -m "feat(proto): conversions between core Event and proto Event"
```

**Self-review checklist:**
- [ ] Build clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -p isengard-proto --all-targets -- -D warnings` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Agent OutboundEmitter

**Files:**
- Create: `crates/isengard-agent/src/events.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (`pub mod events;`)

- [ ] **Step 1: Create the OutboundEmitter**

Create `crates/isengard-agent/src/events.rs`:

```rust
//! Agent-side EventEmitter that queues events onto an mpsc channel for the
//! sync loop to drain and multiplex onto the Sync stream.
//!
//! Backpressure policy: if the channel is full (sync stream is slow, or
//! disconnected), `emit` drops the event and logs a WARN. v1 doesn't persist
//! outbound events.

use async_trait::async_trait;
use isengard_core::{Event, EventEmitter};
use tokio::sync::mpsc;
use tracing::warn;

const OUTBOUND_CAPACITY: usize = 256;

pub struct OutboundEmitter {
    tx: mpsc::Sender<Event>,
}

impl OutboundEmitter {
    /// Returns the emitter and the matching Receiver for the sync loop.
    pub fn new() -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        (Self { tx }, rx)
    }
}

#[async_trait]
impl EventEmitter for OutboundEmitter {
    async fn emit(&self, event: Event) {
        if let Err(e) = self.tx.try_send(event) {
            match e {
                mpsc::error::TrySendError::Full(ev) => {
                    warn!(
                        kind = %ev.kind,
                        "outbound event dropped: channel full"
                    );
                }
                mpsc::error::TrySendError::Closed(ev) => {
                    warn!(
                        kind = %ev.kind,
                        "outbound event dropped: channel closed (sync loop ended)"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ev(kind: &str) -> Event {
        Event {
            kind: kind.into(),
            occurred_at: Utc::now(),
            summary: kind.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn emit_delivers_to_receiver() {
        let (emitter, mut rx) = OutboundEmitter::new();
        emitter.emit(ev("update.success")).await;
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind, "update.success");
    }

    #[tokio::test]
    async fn emit_does_not_block_when_channel_full() {
        let (emitter, _rx) = OutboundEmitter::new();
        // Fill the channel to capacity (don't drain).
        for i in 0..OUTBOUND_CAPACITY {
            emitter.emit(ev(&format!("k{i}"))).await;
        }
        // One more — should drop, not block.
        let extra = ev("overflow");
        let started = std::time::Instant::now();
        emitter.emit(extra).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }
}
```

- [ ] **Step 2: Add `pub mod events;` to lib.rs**

In `crates/isengard-agent/src/lib.rs`, after the other module declarations:

```rust
pub mod events;
```

- [ ] **Step 3: Build + test**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -5
cd ~/Projects/isengard && cargo test -p isengard-agent --lib events:: 2>&1 | tail -10
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/events.rs crates/isengard-agent/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(agent): OutboundEmitter — mpsc-backed EventEmitter for sync stream"
```

---

## Task 3: Wire OutboundEmitter into agent + sync loop

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs` (`run_agent`)
- Modify: `crates/isengard-agent/src/sync.rs` (`run_sync_loop`)

- [ ] **Step 1: Update run_sync_loop to take a Receiver**

In `crates/isengard-agent/src/sync.rs`, find the existing `run_sync_loop` signature. Add a new parameter `events_rx: mpsc::Receiver<Event>`. Inside the loop, extend the `tokio::select!` to read from the receiver and send `AgentMessage::Event(proto_event)` on the outbound stream.

Specific edits:

1. Add imports:

```rust
use isengard_core::Event as CoreEvent;
use isengard_proto::v1::{agent_message, Event as ProtoEvent};
use tokio::sync::mpsc;
```

2. Update the function signature (whatever it currently is):

```rust
pub async fn run_sync_loop(
    // ... existing params ...
    mut events_rx: mpsc::Receiver<CoreEvent>,
) -> anyhow::Result<()> {
```

3. Inside the existing tokio::select!, add a new branch:

```rust
                ev = events_rx.recv() => {
                    let Some(core_ev) = ev else {
                        // Channel closed — agent shutting down.
                        break;
                    };
                    let proto_ev: ProtoEvent = core_ev.into();
                    let msg = isengard_proto::v1::AgentMessage {
                        payload: Some(agent_message::Payload::Event(proto_ev)),
                    };
                    if let Err(e) = outbound_tx.send(msg).await {
                        warn!(error = %e, "failed to send event over sync stream");
                        break;
                    }
                }
```

(Adapt `outbound_tx` to whatever the existing variable name is in the loop — likely something like `tx` or `outbound`.)

Also do the same in `run_sync_with_reconnect` — it needs to plumb the events_rx through to each `run_sync_loop` invocation. But wait — the receiver is consumed by recv. On reconnection, we'd need to keep the same receiver across attempts. Pass `&mut events_rx` into `run_sync_loop` instead of moving.

So change the signature to `events_rx: &mut mpsc::Receiver<CoreEvent>`. Each reconnect-iteration uses the same receiver, so no events are lost across reconnects.

4. Update the outer `run_sync_with_reconnect` to take `&mut events_rx` and pass it through.

- [ ] **Step 2: Update run_agent to build the mpsc + emitter + attach to PluginContext**

In `crates/isengard-agent/src/lib.rs`, find `run_agent`. Before plugin init, build:

```rust
    let (emitter, mut events_rx) = crate::events::OutboundEmitter::new();
    let emitter: Arc<dyn isengard_core::EventEmitter> = Arc::new(emitter);
```

Where the existing PluginContext is constructed for plugin init, change:

```rust
    let ctx = PluginContext::new(HostMode::Agent, plugin_config.clone())
        .with_events(emitter.clone());
```

Pass `&mut events_rx` into the call to `run_sync_with_reconnect` (or wherever the sync loop is launched). It must remain alive for the agent's lifetime — own it in the outer scope; the sync loop borrows.

- [ ] **Step 3: Build + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-agent --lib 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-agent --tests 2>&1 | tail -15
```

Expected: build clean, all unit + integration tests pass (existing 11 unit + the events ones + 2 enroll_e2e + 1 sync_e2e + 1 reconnect_e2e).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/lib.rs crates/isengard-agent/src/sync.rs
cd ~/Projects/isengard && git commit -m "feat(agent): sync loop multiplexes events onto Sync stream + emitter wired to plugins"
```

---

## Task 4: Controller persists + broadcasts on Event variant

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs` (`run_controller`)
- Modify: `crates/isengard-controller/src/service.rs` (Sync handler)

- [ ] **Step 1: ControllerService gains journal + bus**

In `crates/isengard-controller/src/service.rs`, modify `ControllerService` (or whatever the struct is named):

```rust
use std::sync::Arc;
use isengard_storage::{Inventory, Journal, InsertEvent};
use crate::bus::EventBus;

pub struct ControllerService {
    inventory: Arc<Inventory>,
    journal: Arc<Journal>,
    bus: Arc<EventBus>,
}

impl ControllerService {
    pub fn new(inventory: Arc<Inventory>, journal: Arc<Journal>, bus: Arc<EventBus>) -> Self {
        Self { inventory, journal, bus }
    }
}
```

(Keep the existing `inventory` field; just add the two new ones.)

In the Sync RPC handler, find the inbound message-processing loop. The current code handles SyncHello and Heartbeat in a `match`. Add the Event variant:

```rust
                Some(isengard_proto::v1::agent_message::Payload::Event(proto_ev)) => {
                    // host_id is known from the SyncHello flow that opened this stream.
                    let core_ev: isengard_core::Event =
                        match proto_ev.try_into() {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(error = %e, "discarding malformed event");
                                continue;
                            }
                        };
                    let mut to_persist = core_ev.clone();
                    to_persist.host_id = Some(host_id.into());  // host_id from SyncHello
                    let insert = isengard_storage::InsertEvent {
                        host_id: Some(host_id),
                        kind: to_persist.kind.clone(),
                        container_name: to_persist.container_name.clone(),
                        image: to_persist.image.clone(),
                        old_digest: to_persist.old_digest.clone(),
                        new_digest: to_persist.new_digest.clone(),
                        error: to_persist.error.clone(),
                        summary: to_persist.summary.clone(),
                        metadata_json: if to_persist.metadata.is_null() {
                            None
                        } else {
                            Some(to_persist.metadata.to_string())
                        },
                        occurred_at: to_persist.occurred_at,
                    };
                    if let Err(e) = self.journal.insert(insert).await {
                        tracing::warn!(error = %e, "journal.insert failed; dropping event");
                        continue;
                    }
                    self.bus.publish(to_persist);
                }
```

(Adapt `host_id` — it's the storage `HostId` newtype, available from the connection's already-resolved hello frame. The `host_id.into()` converts to `core::HostId` (`ulid::Ulid`) via the From impls added in 4a-T4.)

- [ ] **Step 2: run_controller wires Journal + EventBus**

In `crates/isengard-controller/src/lib.rs`, find `run_controller`. Where it currently builds `Inventory`, also build `Journal` (sharing the same SQLite file) and `EventBus`:

```rust
    let inv_path = state_dir.join("inventory.db");
    let inventory = Arc::new(Inventory::open(&inv_path).await?);
    let journal = Arc::new(Journal::open(&inv_path).await?);
    let bus = Arc::new(EventBus::new());
    let svc = ControllerService::new(inventory, journal, bus);
```

(Both Inventory and Journal point at the same file. Inventory's open ran the migrations during Phase 2b; Journal's open just connects without re-running. Verify in the existing `Inventory::open` that migrations run — they do per Phase 2b. The 0002 migration is included in `migrations/` and runs as part of Inventory's migration step.)

- [ ] **Step 3: Build + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
```

Expected: build clean, existing 12 controller tests still pass.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/lib.rs crates/isengard-controller/src/service.rs
cd ~/Projects/isengard && git commit -m "feat(controller): Sync handler persists events to Journal + broadcasts on EventBus"
```

---

## Task 5: Integration test — agent emits, journal contains

**Files:**
- Create: `crates/isengard-agent/tests/events_e2e.rs`

- [ ] **Step 1: Create the test**

Create `crates/isengard-agent/tests/events_e2e.rs`:

```rust
//! Integration test: agent emits an event via its EventEmitter, the
//! controller persists it to the journal, and a bus subscriber sees it.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use isengard_agent::events::OutboundEmitter;
use isengard_core::{Event, EventEmitter};
use tempfile::tempdir;

// This test reuses the in-process controller spin-up from enroll_e2e.rs
// (or sync_e2e.rs, whichever exists). Since those helpers are test-only,
// we open-code a minimal version here. The pattern: bind controller on a
// random port, run agent against it, tear down. Adapt to whatever helper
// exists in tests/common/ if one is present.

#[tokio::test]
async fn agent_emitted_event_lands_in_controller_journal() {
    // 1. Start controller in-process.
    let ctrl_dir = tempdir().unwrap();
    let agent_dir = tempdir().unwrap();

    let listen = "127.0.0.1:0";  // ephemeral port — but controller takes a string;
                                  // adapt to whatever the existing helper does.

    // (See existing test files for the harness pattern. The exact spin-up code
    // depends on whether enroll_e2e.rs uses a helper module or inlines.)
    //
    // For brevity here, the implementer should:
    //  - Look at crates/isengard-agent/tests/enroll_e2e.rs for the harness pattern
    //  - Copy/adapt that harness for this test
    //  - After the agent is enrolled and the sync stream is up, get a handle
    //    to the agent's EventEmitter (via PluginContext.events) — actually
    //    that's hard from outside. Easier: construct an OutboundEmitter
    //    directly and feed it to a custom AgentOptions.with_events_receiver
    //    if such a hook exists, OR use an internal fork.
    //
    // Simplest viable path: make this test exercise OutboundEmitter +
    // run_sync_loop directly without going through run_agent. That's still
    // a true integration of the wire path.

    // Implementer: pick the simplest viable harness given what's already in
    // the agent's test files. The assertion to make:
    //   1. emitter.emit(some_event).await
    //   2. wait up to 2s
    //   3. inspect controller's journal: should contain the event
    //
    // To inspect the journal, you can either:
    //   - open the sqlite file directly via Journal::open
    //   - OR add a helper RPC (out of scope)
    //
    // Recommend: open the journal file directly with Journal::open and call list_recent.

    let _ = ctrl_dir;
    let _ = agent_dir;
    let _ = listen;
    let _ = Utc::now();
    let _: Box<dyn EventEmitter> = Box::new(OutboundEmitter::new().0);
    let _: Event = Event::default();
    let _ = Arc::new(42);
}
```

**Implementer note:** the test above is a SCAFFOLD. Implement it concretely using the same pattern as `crates/isengard-agent/tests/enroll_e2e.rs` or `sync_e2e.rs` (whichever has the cleanest controller spin-up). The end-state assertion: after agent emits, `Journal::open(controller_state_dir.join("inventory.db")).list_recent(10)` returns at least one event matching the emitted kind.

If wiring this end-to-end is too involved for one task and risks breaking other things, simplify to: bring up controller, manually push an event proto onto an active Sync stream client, assert journal has it. The point is to prove the path: wire → handler → journal → bus.

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent --test events_e2e 2>&1 | tail -20
```

Expected: passes.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/tests/events_e2e.rs
cd ~/Projects/isengard && git commit -m "test(agent): e2e — emitted event lands in controller journal"
```

---

## Task 6: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4b`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 112 baseline + 2 events + 1 e2e = 115. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4b -m "phase 4b: agent emits + controller persists & broadcasts"
cd ~/Projects/isengard && git tag -l | grep phase4b
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 112 baseline + new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Tag `v0.1.0-alpha.phase4b` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§2-§6) | Plan task |
|---|---|
| Agent emits via EventEmitter on PluginContext | Tasks 2-3 |
| Sync stream multiplexes events | Task 3 |
| Controller persists to Journal | Task 4 |
| Controller broadcasts to EventBus | Task 4 |
| End-to-end wire test | Task 5 |

No producers (updater) or consumers (notifier) yet — those are 4c + 4d.

**Type consistency check:**
- `OutboundEmitter` impl `EventEmitter` — works because `Event` is the same type both sides.
- Conversions `From<CoreEvent> for ProtoEvent` + `TryFrom<ProtoEvent> for CoreEvent` defined in proto crate.
- `host_id: HostId` from storage converts to `Option<ulid::Ulid>` via the From impls added in 4a-T4.
- `Arc<dyn EventEmitter>` lives in PluginContext; agent attaches it via `with_events`.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4b-agent-emit-controller-broadcast.md`. Subagent-driven execution.
