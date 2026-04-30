# Phase 2f: Reconnection + Exponential Backoff

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** When the agent's Sync stream dies (controller restart, network blip, transient failure), the agent reconnects automatically with jittered exponential backoff. After the stream is stable for ≥ 60s, backoff resets so the next failure starts from 1s again. End state: kill the controller mid-flight, restart it, the agent reconnects on its own and resumes heartbeating without re-enrollment.

**Architecture:** A new `backoff` module gives us a small `Backoff` type with `next_delay()` and `reset()` semantics. `run_sync_loop` from Phase 2e becomes the inner-loop body inside a new `run_sync_with_reconnect` outer loop:

1. Compute next delay (1s on first attempt, exponential growth on each successive failure)
2. `tokio::select!` between `cancel.notified()` and `tokio::time::sleep(delay)`
3. Try `run_sync_loop` once
4. On Ok (stream closed via cancel) → return Ok
5. On Err (stream errored) → check whether the previous attempt was alive ≥ 60s; if yes, reset backoff to 1s; loop

Stream-stable detection: capture `Instant::now()` before calling `run_sync_loop`; on Err, if `elapsed >= Duration::from_secs(60)`, reset.

The reconnection logic lives in `isengard-agent/src/sync.rs` next to the existing `run_sync_loop`. `run_agent` calls `run_sync_with_reconnect` instead.

**Spec:** §6 agent lifecycle (backoff: 1s → 60s, ±20% jitter, reset after 60s stable), §13 sub-phase 2f.

---

## Scope

**In:**
- `backoff` module: `Backoff` struct with `new`, `next_delay() -> Duration`, `reset()`, plus jitter (±20%)
- Property tests on `Backoff`: monotonic until cap, never exceeds 60s × 1.2, resets to base after `reset()`
- `run_sync_with_reconnect`: outer loop wrapping `run_sync_loop`, tracks stream uptime, resets backoff on long-stable streams
- `run_agent` calls the reconnect-aware version
- New e2e test: agent survives controller restart (kill controller, restart it, verify agent reconnects within ~10s and continues heartbeating)

**Out:**
- "Agent unreachable" event journaling (Phase 4 — needs the journal)
- Goodbye message on graceful shutdown (defer; not blocking)
- Per-RPC retry policy (the stream-level reconnect is sufficient for v1; tonic doesn't have built-in retry)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 49 tests
3. `just ci-local` clean
4. Manual smoke: start controller, run agent, kill controller, restart it within 10s, verify agent reconnects and heartbeats resume
5. Tag `v0.1.0-alpha.phase2f` set locally

---

## File Structure

```
isengard/
├── crates/
│   ├── isengard-agent/
│   │   ├── src/
│   │   │   ├── backoff.rs                                # CREATE: Backoff type + tests
│   │   │   ├── sync.rs                                   # MODIFY: + run_sync_with_reconnect
│   │   │   └── lib.rs                                    # MODIFY: mod backoff; run_agent uses run_sync_with_reconnect
│   │   └── tests/
│   │       └── reconnect_e2e.rs                          # CREATE: agent survives controller restart
```

---

## Task 1: `backoff` module

**Files:**
- Create: `crates/isengard-agent/src/backoff.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (`mod backoff;`)

- [ ] **Step 1: Write `backoff.rs`**

```bash
cat > crates/isengard-agent/src/backoff.rs <<'EOF'
//! Jittered exponential backoff for the agent's reconnect loop.
//!
//! Spec §6: "1s, 2s, 4s, 8s, 16s, 32s, 60s (cap), jittered ±20%". Reset to
//! base after a stream stays open ≥ 60s.

use std::time::Duration;

const BASE_MS: u64 = 1_000;
const CAP_MS: u64 = 60_000;
const JITTER_PCT: f64 = 0.20;

#[derive(Debug, Clone)]
pub struct Backoff {
    /// Current attempt number (0 = first attempt, no backoff yet).
    attempt: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    /// Compute the delay for the next attempt and increment the counter.
    /// Returns Duration::ZERO on the very first call (attempt 0).
    pub fn next_delay(&mut self) -> Duration {
        let attempt = self.attempt;
        self.attempt = self.attempt.saturating_add(1);

        if attempt == 0 {
            return Duration::ZERO;
        }

        // Exponential: base * 2^(attempt-1), capped at CAP.
        let exp = (attempt - 1).min(20); // 2^20 is way past the cap; saturate
        let unjittered_ms = BASE_MS.saturating_mul(1u64 << exp).min(CAP_MS);

        // Apply ±20% jitter using a tiny LCG so we don't pull rand as a dep
        // here. The randomness only needs to be "good enough" — not crypto.
        let jitter = jitter_factor();
        let jittered_ms = ((unjittered_ms as f64) * jitter) as u64;
        // Clamp to [1, CAP * (1 + JITTER_PCT)] to avoid 0-ms pathological case.
        let final_ms = jittered_ms.max(1).min(((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u64);

        Duration::from_millis(final_ms)
    }

    /// Reset the counter so the next call to `next_delay` returns ZERO.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Current attempt number (0 = haven't attempted yet).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a value in `[1.0 - JITTER_PCT, 1.0 + JITTER_PCT]` using a tiny
/// thread-local LCG. Cheap, deterministic-per-call (advances on each call),
/// no extra crate.
fn jitter_factor() -> f64 {
    use std::cell::Cell;
    thread_local! {
        // Seed from process start time. Different threads get different
        // seeds because thread_local default-initializes per-thread.
        static STATE: Cell<u64> = Cell::new({
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdeadbeef)
                | 1 // odd seed
        });
    }
    STATE.with(|s| {
        // LCG params from Numerical Recipes (Park-Miller-ish).
        let next = s.get().wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        s.set(next);
        let normalized = (next >> 32) as f64 / u32::MAX as f64; // [0, 1)
        1.0 - JITTER_PCT + 2.0 * JITTER_PCT * normalized // [1-J, 1+J)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_returns_zero() {
        let mut b = Backoff::new();
        assert_eq!(b.next_delay(), Duration::ZERO);
    }

    #[test]
    fn second_call_is_around_base() {
        let mut b = Backoff::new();
        let _ = b.next_delay(); // attempt 0 → 0
        let d = b.next_delay(); // attempt 1 → ~1s ± 20%
        let ms = d.as_millis() as u64;
        assert!(ms >= 800, "expected ≥800ms, got {ms}ms");
        assert!(ms <= 1200, "expected ≤1200ms, got {ms}ms");
    }

    #[test]
    fn grows_until_cap() {
        let mut b = Backoff::new();
        let mut last = Duration::ZERO;
        for i in 0..20 {
            let d = b.next_delay();
            // d should be monotonically non-decreasing once we leave attempt 0
            if i > 0 {
                assert!(
                    d >= last || d.as_millis() >= (CAP_MS as u128 * 80 / 100),
                    "step {i}: {d:?} < {last:?}"
                );
            }
            last = d;
        }
        // Final delay must be ≤ cap × (1 + jitter)
        let max = ((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u128;
        assert!(last.as_millis() <= max, "final {last:?} exceeds cap {max}ms");
    }

    #[test]
    fn never_exceeds_cap_plus_jitter() {
        let mut b = Backoff::new();
        let cap = ((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u128;
        for _ in 0..50 {
            assert!(b.next_delay().as_millis() <= cap);
        }
    }

    #[test]
    fn reset_returns_to_zero_for_next_call() {
        let mut b = Backoff::new();
        for _ in 0..10 {
            let _ = b.next_delay();
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::ZERO);
    }

    #[test]
    fn jitter_factor_in_expected_range() {
        for _ in 0..1000 {
            let f = jitter_factor();
            assert!(f >= 1.0 - JITTER_PCT, "got {f}");
            assert!(f <= 1.0 + JITTER_PCT, "got {f}");
        }
    }
}
EOF
```

- [ ] **Step 2: Wire `mod backoff;` into lib.rs**

Add `pub mod backoff;` near the other module declarations in `~/Projects/isengard/crates/isengard-agent/src/lib.rs`.

- [ ] **Step 3: Verify build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent --lib backoff:: 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 6 backoff tests pass. Clippy clean.

If a clippy lint fires on the LCG arithmetic (`wrapping_mul`, `wrapping_add`), it's likely an `arithmetic_side_effects` complaint — the wrapping ops are deliberate. Add `#[allow(clippy::cast_precision_loss)]` or similar if the lint is specific. Most likely no issue.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/backoff.rs crates/isengard-agent/src/lib.rs && git commit -m "feat(agent): backoff module — jittered exponential backoff with reset"
```

---

## Task 2: `run_sync_with_reconnect` outer loop

**Files:**
- Modify: `crates/isengard-agent/src/sync.rs`

- [ ] **Step 1: Add the reconnect-aware function**

Open `crates/isengard-agent/src/sync.rs`. Append (BEFORE the `#[cfg(test)]` if any, after the `run_sync_loop` function):

```rust
use crate::backoff::Backoff;
use std::time::Instant;

/// Run the Sync loop with automatic reconnection on stream failure. Returns
/// Ok only when `cancel` fires (graceful shutdown). On stream error, sleeps
/// per the backoff policy and retries.
///
/// Backoff resets to base if the previous attempt's stream stayed open ≥ 60s
/// (proves the connection was healthy).
#[instrument(skip(token, cancel), fields(agent_id = %agent_id))]
pub async fn run_sync_with_reconnect(
    controller_url: String,
    token: String,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let mut backoff = Backoff::new();
    const STABLE_THRESHOLD: Duration = Duration::from_secs(60);

    loop {
        let delay = backoff.next_delay();
        if delay > Duration::ZERO {
            info!(
                attempt = backoff.attempt(),
                delay_ms = delay.as_millis() as u64,
                "waiting before sync reconnect"
            );
            tokio::select! {
                _ = cancel.notified() => {
                    info!("cancel during backoff, exiting");
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }

        let attempt_started = Instant::now();
        let result = run_sync_loop(
            controller_url.clone(),
            token.clone(),
            agent_id.clone(),
            interval_secs,
            cancel.clone(),
        )
        .await;

        match result {
            Ok(()) => {
                // Graceful cancel — exit.
                info!("sync loop exited cleanly via cancel");
                return Ok(());
            }
            Err(e) => {
                let elapsed = attempt_started.elapsed();
                if elapsed >= STABLE_THRESHOLD {
                    info!(
                        elapsed_secs = elapsed.as_secs(),
                        "stream was stable; resetting backoff"
                    );
                    backoff.reset();
                }
                tracing::warn!(error = %e, attempt = backoff.attempt(), "sync stream error, will retry");
                // Loop continues; cancel-during-backoff is handled at the top.
            }
        }
    }
}
```

You'll need imports at the top of `sync.rs`:

```rust
use crate::backoff::Backoff;  // already shown above
use std::time::Instant;
```

The existing `Duration`, `Arc`, `info`, `instrument`, `Result` imports stay.

- [ ] **Step 2: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -3
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 3: Run existing agent tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent 2>&1 | tail -10
```

Expected: 13 tests pass (1 dummy + 4 state + 6 backoff + 2 e2e). The `run_sync_with_reconnect` function isn't yet called by `run_agent` — Task 3 wires that.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/sync.rs && git commit -m "feat(agent): run_sync_with_reconnect — backoff retry, stable-stream reset"
```

---

## Task 3: `run_agent` uses reconnect-aware sync

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Swap the call**

In `~/Projects/isengard/crates/isengard-agent/src/lib.rs`, find the spawn that calls `sync::run_sync_loop(...)`. Replace `run_sync_loop` with `run_sync_with_reconnect`:

```rust
    let sync_handle = {
        let url = opts.controller_url.clone();
        let agent_id = agent_id.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            sync::run_sync_with_reconnect(url, token, agent_id, HEARTBEAT_INTERVAL_SECS, cancel).await
        })
    };
```

That's the only change. Everything else (ctrl_c await, drain timeout, plugin stop) stays.

- [ ] **Step 2: Verify build + clippy + existing tests**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -3
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
cd ~/Projects/isengard && cargo test -p isengard-agent 2>&1 | tail -10
```

Expected: 13 tests pass. The existing 3 e2e tests (enroll_e2e × 2 + sync_e2e × 1) still pass — the reconnect wrapper is invisible on the happy path.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/lib.rs && git commit -m "feat(agent): run_agent uses run_sync_with_reconnect"
```

---

## Task 4: Reconnection e2e test — agent survives controller restart

**Files:**
- Create: `crates/isengard-agent/tests/reconnect_e2e.rs`

This is the canonical proof that Phase 2f works: kill the controller, the agent retries, restart the controller, the agent reconnects and resumes heartbeating.

- [ ] **Step 1: Write the test**

```bash
cat > crates/isengard-agent/tests/reconnect_e2e.rs <<'EOF'
//! Phase 2f e2e: agent survives controller restart.
//!
//! Strategy: spawn controller A on a fixed port, let agent enroll + heartbeat
//! for a bit, abort controller A, spawn controller B on the SAME port (with
//! the SAME state dir, so it has the agent's host row), wait for agent to
//! reconnect, verify last_seen_at advances after the gap.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use isengard_agent::{run_agent, AgentOptions};
use isengard_controller::{run_controller, ControllerOptions};
use isengard_storage::Inventory;
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2f";

/// Spawn a controller on a SPECIFIC port (so a restart can re-bind).
/// Returns a JoinHandle so the caller can `.abort()` to "kill" the controller.
async fn spawn_controller_on(
    port: u16,
    state_dir: PathBuf,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    unsafe {
        std::env::set_var("ISENGARD_TOKEN", TEST_TOKEN);
    }

    let handle = tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir,
            config: serde_json::Value::Object(Default::default()),
        })
        .await;
    });

    // Wait for bind.
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return (addr, handle);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

/// Pick a free port by binding 0, then drop the listener.
fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_survives_controller_restart() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();
    let port = pick_free_port();

    // Spawn controller A.
    let (addr, ctrl_a) = spawn_controller_on(port, controller_state.path().to_path_buf()).await;

    // Spawn the agent against `addr`.
    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    };
    let agent_handle = tokio::spawn(run_agent(opts));

    // Wait for enrollment.
    let state_path = agent_state.path().join("agent.json");
    for _ in 0..50 {
        if state_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(state_path.exists(), "agent should enroll within 5s");

    // Wait for the first heartbeat to land (last_seen_at non-null).
    let inv_path = controller_state.path().join("isengard.db");
    let mut first_last_seen = None;
    for _ in 0..120 {
        let inv = Inventory::open(&inv_path).await.unwrap();
        let hosts = inv.list_hosts().await.unwrap();
        if let Some(ts) = hosts.first().and_then(|h| h.last_seen_at) {
            first_last_seen = Some(ts);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let initial_ts = first_last_seen.expect("first heartbeat must land within 30s");

    // Kill controller A. Agent's stream will error; reconnect loop kicks in.
    ctrl_a.abort();
    let _ = ctrl_a.await;
    // Give the OS a moment to release the port.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Spawn controller B on the same port + same state dir. Same agent_id
    // already in the hosts table.
    let (_addr_b, _ctrl_b) =
        spawn_controller_on(port, controller_state.path().to_path_buf()).await;

    // Wait up to 30s for last_seen_at to advance from `initial_ts`. The agent
    // backoff starts at ~1s; reconnection should land in ~1-3s, then the next
    // heartbeat fires up to 10s later, then touch_host runs.
    let mut advanced = false;
    for _ in 0..120 {
        let inv = Inventory::open(&inv_path).await.unwrap();
        let hosts = inv.list_hosts().await.unwrap();
        if let Some(ts) = hosts.first().and_then(|h| h.last_seen_at) {
            if ts > initial_ts {
                advanced = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        advanced,
        "agent should reconnect to controller B and advance last_seen_at within 30s"
    );

    agent_handle.abort();
}
EOF
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent --test reconnect_e2e 2>&1 | tail -10
```

Expected: 1 test passes in ~15-30s wall clock. The first heartbeat takes ~10s, the controller swap takes ~1s, reconnection happens within a few seconds, the next heartbeat is up to 10s after that.

If the test times out at 30s without seeing last_seen_at advance:
- Verify the controller restart actually opened the port (sometimes the OS holds it briefly; the 500ms sleep should cover that, bump to 1000ms if needed)
- Verify `run_sync_with_reconnect` is actually being called by `run_agent` (Task 3 should have wired it)
- Check the agent logs (run with `--log=debug` manually if needed) for "sync stream error, will retry" lines

If the test takes > 60s, increase the second polling loop (it's 120 × 250ms = 30s, can bump to 240 × 250ms = 60s).

If the test passes in <5s, that's surprising — suggests the controller swap was instantaneous. Might mean the agent never actually disconnected. Check that `ctrl_a.abort()` actually terminates the controller. If the test's behavior is suspicious, investigate; otherwise accept the speedy pass.

- [ ] **Step 3: Run full workspace tests**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: ≥ 49 tests (45 from Phase 2e + 4 = 6 backoff + 1 reconnect_e2e − 2 unchanged sync_e2e but I miscounted; just verify ≥ 49 and no FAILED). Actually: 45 + 6 backoff + 1 reconnect_e2e = 52. Even better.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/tests/reconnect_e2e.rs && git commit -m "test(agent): e2e — agent survives controller restart"
```

---

## Task 5: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails, apply `cargo fmt` + commit as `style: cargo fmt across phase 2f`, re-run.

- [ ] **Step 2: Manual smoke run**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-2f-ctrl /tmp/isengard-2f-agent

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9450 --state-dir /tmp/isengard-2f-ctrl &
CTRL_A=$!
sleep 1

ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9450 --state-dir /tmp/isengard-2f-agent 2>&1 | tee /tmp/isengard-2f-agent.log &
AGENT=$!
sleep 12  # let one heartbeat land

echo "--- killing controller A ---"
kill $CTRL_A
wait $CTRL_A 2>/dev/null || true
sleep 2

echo "--- starting controller B (same port, same state-dir) ---"
ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9450 --state-dir /tmp/isengard-2f-ctrl &
CTRL_B=$!
sleep 12  # let agent reconnect and one more heartbeat land

echo "--- agent log tail (look for reconnect) ---"
tail -20 /tmp/isengard-2f-agent.log

echo "--- final last_seen_at ---"
which sqlite3 >/dev/null && sqlite3 /tmp/isengard-2f-ctrl/isengard.db \
  "SELECT hostname, last_seen_at, datetime(last_seen_at, 'unixepoch') FROM hosts;" || echo "(sqlite3 not available)"

kill -INT $AGENT
wait $AGENT 2>/dev/null || true
kill $CTRL_B 2>/dev/null
wait $CTRL_B 2>/dev/null || true
```

Expected:
- Agent log shows `sync stream error, will retry` after controller A dies, then `Sync stream opened` again after controller B starts
- `last_seen_at` is recent (within ~10s of the script ending)

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2f -m "phase 2f: reconnection + jittered backoff — agent survives controller restart"
cd ~/Projects/isengard && git tag -l | grep phase2f
```

Don't push.

## Self-review

| Spec requirement | Plan task |
|---|---|
| §6 backoff: 1s → 60s, ±20% jitter | Task 1 |
| §6 reset to base after stable stream ≥ 60s | Task 2 |
| §10 crash test: agent reconnects after controller restart | Task 4 |

No placeholders. Phase 2g (test harness extraction) is genuinely housekeeping and ships separately.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-2f-reconnection.md`. Subagent-driven execution.
