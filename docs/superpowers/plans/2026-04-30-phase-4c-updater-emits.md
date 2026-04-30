# Phase 4c: Updater Emits update.* Events

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** The updater plugin produces real journal events. End state: every cycle emits `update.checked` (cycle summary), and per-container actions emit `update.success` (recreate worked) or `update.failed` (recreate errored). The self-update path emits `update.success` just before scheduling its exit. Once 4c lands, the journal sees actual updater activity, and 4d's notifier will have something meaningful to forward to Telegram.

**Architecture:** The `Updater` struct gains an `emitter: Option<Arc<dyn EventEmitter>>` field, populated from `PluginContext.events` in `init`. The spawned cycle task clones the emitter and threads it into `do_cycle`. New helper `emit(emitter, event)` no-ops when the emitter is None (controller-side or unit tests). Each emission carries the structured fields the spec requires (kind, occurred_at, summary, container_name, image, digests, error). The self-update path in `self_update::update_self` emits success synchronously before the spawned exit task fires, so the event reaches the controller before the agent dies.

**Tech stack:** No new workspace deps. Uses `chrono::Utc::now()` for `occurred_at`.

**Branch:** `next`. Lefthook pre-push runs full gates (cargo-deny mandatory).

**Spec:** `docs/superpowers/specs/2026-04-30-phase-4-events-journal-design.md` §1-§5 + parent spec §9.1 (event kinds: `update.checked`, `update.success`, `update.failed`, `update.skipped`).

---

## Scope

**In:**
- `Updater::emitter: Option<Arc<dyn EventEmitter>>` field, populated in `init` from `ctx.events`.
- Cycle emits:
  - **`update.success`** after `recreate::update_container` returns Ok. Fields: `container_name`, `image`, `old_digest` (the local digest before pull), `new_digest` (the remote digest), `summary = "updated <container> to <new_digest>"`.
  - **`update.failed`** after `recreate::update_container` returns Err. Fields: `container_name`, `image`, `error`, `summary = "update failed for <container>: <error>"`.
  - **`update.checked`** at the end of every cycle, ONCE per cycle as a summary. Fields: `summary = "cycle: candidates=N up_to_date=M needs_update=K unknown=L"`, `metadata` JSON containing the four counts.
- Self-update emits:
  - **`update.success`** in `self_update::update_self` after the replacement starts (before scheduling `process::exit(0)`). Fields: `container_name = self_name`, `image = new_image`, `summary = "self-update complete: <self_name> → <new_image>"`.
- Helper `async fn emit(emitter: Option<&Arc<dyn EventEmitter>>, event: Event)` — no-op when emitter is None, awaits emitter.emit(event) otherwise. Lives in `lib.rs` (private).
- Unit tests via a `RecordingEmitter` mock (in test module) that captures emitted events; verify cycle emits the right kinds with the right fields when given seeded inputs. Pure-function-style — no Docker required.
- Don't break: existing `recreate_e2e`, `cycle_e2e`, `plugin_loads`, `registry_e2e` integration tests.

**Out (deferred):**
- `update.checked` per-container (we emit only the cycle summary; per-container would be too noisy for v1)
- `update.skipped` event kind — fuzzy semantics, no notifier consumer demands it yet; spec says "may emit", v1.x can revisit
- Notifier consumption (4d)
- `agent.disconnect_long` (4f)
- Per-event metadata JSON beyond the cycle summary (v1.x)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 115 baseline + new unit tests
3. `just ci-local` clean (now includes cargo-deny mandatory)
4. Tag `v0.1.0-alpha.phase4c` set locally
5. **Not pushed** until user confirms

---

## File Structure

```
crates/isengard-plugins/updater/
└── src/
    ├── lib.rs              # MODIFY: emitter field + helper + cycle emits + new unit tests
    ├── self_update.rs      # MODIFY: emit update.success before exit
    └── (others unchanged)
```

---

## Task 1: Updater holds emitter + helper + emit on success/failure

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs`

- [ ] **Step 1: Add emitter field, init populates from ctx.events**

In `crates/isengard-plugins/updater/src/lib.rs`:

1. Add to imports:

```rust
use chrono::Utc;
use isengard_core::{Event, EventEmitter};
```

(`Event` and `EventEmitter` are re-exported from `isengard_core` per Phase 4a-T4. The existing imports already pull `PluginContext`, `HostMode`, etc.)

2. Add field to `Updater`:

```rust
pub struct Updater {
    docker: Option<Docker>,
    registry: Option<Arc<RegistryClient>>,
    cycle_interval: Duration,
    emitter: Option<Arc<dyn EventEmitter>>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}
```

3. Update `Updater::new()`:

```rust
impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            registry: None,
            cycle_interval: Duration::from_secs(DEFAULT_CYCLE_INTERVAL_SECS),
            emitter: None,
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}
```

4. In `init`, after the existing config-reading + docker connect + registry construction, populate emitter from ctx:

```rust
        // Pick up the agent's EventEmitter (None if running on controller side
        // or in a test that didn't wire one).
        self.emitter = ctx.events.clone();
        if self.emitter.is_some() {
            info!("updater wired to event emitter");
        }
```

5. Add a private helper near the top of the file (after the constants):

```rust
async fn emit(emitter: Option<&Arc<dyn EventEmitter>>, event: Event) {
    if let Some(e) = emitter {
        e.emit(event).await;
    }
}
```

- [ ] **Step 2: Thread emitter into the spawned cycle task**

Update `start` to clone `self.emitter` into the closure:

```rust
    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let registry = self
            .registry
            .clone()
            .ok_or_else(|| start_err("updater started before init"))?;
        let emitter = self.emitter.clone();
        let cancel = self.cancel.clone();
        let interval = self.cycle_interval;

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(&docker, &registry, emitter.as_ref()).await {
                            warn!(error = %e, "updater cycle failed");
                        }
                    }
                }
            }
        });

        self.task = Some(task);
        info!("updater started");
        Ok(())
    }
```

Update `AgentPlugin::run_cycle`:

```rust
#[async_trait]
impl AgentPlugin for Updater {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| init_err("run_cycle before init"))?;
        do_cycle(docker, registry, self.emitter.as_ref())
            .await
            .map_err(|e| init_err(format!("cycle failed: {e}")))
    }
}
```

- [ ] **Step 3: Update do_cycle to emit success/failed/checked**

Change the signature:

```rust
async fn do_cycle(
    docker: &Docker,
    registry: &RegistryClient,
    emitter: Option<&Arc<dyn EventEmitter>>,
) -> anyhow::Result<()> {
```

Inside the existing match-arm for `(Some(local), Some(remote))` (the non-equal case = `needs_update`), find where Phase 3d added the `recreate::update_container` call. Replace that block:

```rust
                let Some(container_id) = c.id.as_deref() else {
                    warn!(container = %name, "no container ID; cannot update");
                    continue;
                };

                match recreate::update_container(docker, container_id, &image_ref).await {
                    Ok(()) => {
                        emit(
                            emitter,
                            Event {
                                kind: "update.success".into(),
                                occurred_at: Utc::now(),
                                summary: format!("updated {name} to {remote}"),
                                container_name: Some(name.clone()),
                                image: Some(image_str.to_string()),
                                old_digest: Some(local.to_string()),
                                new_digest: Some(remote.to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        warn!(container = %name, error = %err_str, "update failed");
                        emit(
                            emitter,
                            Event {
                                kind: "update.failed".into(),
                                occurred_at: Utc::now(),
                                summary: format!("update failed for {name}: {err_str}"),
                                container_name: Some(name.clone()),
                                image: Some(image_str.to_string()),
                                error: Some(err_str),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                }
```

(Replace the existing `if let Err(e) = recreate::update_container(...) { warn!(...) }` from 3c-T5 / 3d-T3 with this. Keep the self-update branch unchanged — Task 2 of THIS plan handles its emit.)

At the END of `do_cycle`, after the existing aggregate `info!(candidates, up_to_date, needs_update, unknown, "updater cycle complete")` call, add the cycle-summary emit:

```rust
    emit(
        emitter,
        Event {
            kind: "update.checked".into(),
            occurred_at: Utc::now(),
            summary: format!(
                "cycle: candidates={} up_to_date={} needs_update={} unknown={}",
                candidates.len(),
                up_to_date,
                needs_update,
                unknown
            ),
            metadata: serde_json::json!({
                "candidates": candidates.len(),
                "up_to_date": up_to_date,
                "needs_update": needs_update,
                "unknown": unknown,
            }),
            ..Default::default()
        },
    )
    .await;
    Ok(())
}
```

- [ ] **Step 4: Add unit tests with a RecordingEmitter mock**

In the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `lib.rs` (or add one if none exists), add:

```rust
#[cfg(test)]
mod emit_tests {
    use super::*;
    use std::sync::Mutex;

    /// Captures emitted events for assertion.
    struct RecordingEmitter {
        events: Mutex<Vec<Event>>,
    }

    impl RecordingEmitter {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn snapshot(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventEmitter for RecordingEmitter {
        async fn emit(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn emit_helper_skips_when_emitter_none() {
        // Should not panic — no emitter, no-op.
        emit(None, Event::default()).await;
    }

    #[tokio::test]
    async fn emit_helper_delivers_when_emitter_some() {
        let recorder = Arc::new(RecordingEmitter::new());
        let as_emitter: Arc<dyn EventEmitter> = recorder.clone();
        emit(
            Some(&as_emitter),
            Event {
                kind: "test.kind".into(),
                summary: "hello".into(),
                ..Default::default()
            },
        )
        .await;
        let snap = recorder.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].kind, "test.kind");
        assert_eq!(snap[0].summary, "hello");
    }
}
```

- [ ] **Step 5: Build + test + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -15
```

Expected: clean. Lib test count grows from 45 to 47 (+2 emit_tests).

- [ ] **Step 6: Confirm integration tests still pass**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --tests 2>&1 | tail -15
```

Expected: all 4 integration tests pass (or skip if no Docker).

- [ ] **Step 7: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): emit update.success / update.failed / update.checked events"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] Lib tests +2 (emit_tests pass)
- [ ] Integration tests still pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Self-update emits update.success before exit

**Files:**
- Modify: `crates/isengard-plugins/updater/src/self_update.rs`

- [ ] **Step 1: Update update_self signature + add emit before exit**

The current `update_self(docker, self_id, new_image_ref)` signature gains an `emitter: Option<&Arc<dyn EventEmitter>>` parameter. Update the signature and the call site in `do_cycle`:

In `crates/isengard-plugins/updater/src/self_update.rs`:

1. Add imports:

```rust
use chrono::Utc;
use isengard_core::{Event, EventEmitter};
```

2. Update the function signature:

```rust
pub async fn update_self(
    docker: &Docker,
    self_id: &str,
    new_image_ref: &ImageRef,
    emitter: Option<&Arc<dyn EventEmitter>>,
) -> anyhow::Result<()> {
```

3. Right after the existing `info!("self-update complete; exiting current process in 200ms");` line (and BEFORE the `tokio::spawn` that schedules exit), add the emit:

```rust
    if let Some(e) = emitter {
        // Synchronous emit so the event hits the wire before our process dies.
        e.emit(Event {
            kind: "update.success".into(),
            occurred_at: Utc::now(),
            summary: format!("self-update complete: {original_name} → {new_image_str}"),
            container_name: Some(original_name.clone()),
            image: Some(new_image_str.clone()),
            ..Default::default()
        })
        .await;
    }
```

(`original_name` and `new_image_str` are already in scope from earlier in the function.)

- [ ] **Step 2: Update the call site in do_cycle**

In `crates/isengard-plugins/updater/src/lib.rs`, find the self-update branch in `do_cycle` (Phase 3d):

```rust
                        match self_update::update_self(docker, &self_id, &image_ref).await {
```

Change to pass the emitter:

```rust
                        match self_update::update_self(docker, &self_id, &image_ref, emitter).await {
```

- [ ] **Step 3: Build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --lib 2>&1 | tail -10
```

Expected: clean. Lib tests still pass (47 incl. 3 self_update tests + 2 new emit_tests).

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/self_update.rs crates/isengard-plugins/updater/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(updater): self-update emits update.success before scheduling exit"
```

**Self-review checklist:**
- [ ] Build + clippy clean
- [ ] All lib tests pass
- [ ] `cargo fmt --check` clean
- [ ] No `Co-Authored-By` trailer

---

## Task 3: CI gate + tag

- [ ] **Step 1: `just ci-local`** (now includes cargo-deny — won't fall through silently)

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 4c`, re-run.

- [ ] **Step 2: Confirm test counts**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 115 baseline + 2 emit_tests = 117. Critical: zero failures.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase4c -m "phase 4c: updater emits update.success / update.failed / update.checked"
cd ~/Projects/isengard && git tag -l | grep phase4c
```

Don't push.

- [ ] **Step 4: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 115 baseline + new tests, zero failures
- [ ] `just ci-local` clean (with cargo-deny mandatory)
- [ ] Tag `v0.1.0-alpha.phase4c` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (parent §9.1, sub §1-§5) | Plan task |
|---|---|
| Updater emits `update.success` | Task 1 (cycle) + Task 2 (self-update) |
| Updater emits `update.failed` | Task 1 (cycle) |
| Updater emits `update.checked` | Task 1 (cycle summary at end of do_cycle) |
| Events flow through PluginContext.events to wire | Wired in 4b; this phase populates the producer |

`update.skipped` deferred — no current consumer wants it; v1.x can add when notifier or scheduler grows a use case.

**Type consistency check:**
- `emitter: Option<Arc<dyn EventEmitter>>` field on Updater — read in init from `ctx.events.clone()` (which is `Option<Arc<dyn EventEmitter>>` per 4a-T4).
- `do_cycle` takes `Option<&Arc<dyn EventEmitter>>` — passed as `emitter.as_ref()` from the cloned-into-task value.
- `emit` helper takes `Option<&Arc<dyn EventEmitter>>` — same shape, no allocation.
- `Event::default()` works (chrono DateTime<Utc> default = epoch, fine for the no-emit-test path).
- `update_self` signature change ripples to one call site in `do_cycle` — fixed inline.

**No new workspace deps.** All types come from `isengard-core` (already a dep) + `chrono` (already a dep) + `serde_json` (already a dep via existing usage).

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-4c-updater-emits.md`. Subagent-driven execution.
