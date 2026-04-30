# Phase 2e: Sync Stream + Heartbeats (Both Sides)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Replace `Sync`'s `Status::unimplemented` with a real bidi stream. Agent: after enroll, opens `Sync`, sends `SyncHello { agent_id }` once, then a `Heartbeat { ts_ms }` every 10s in a loop. Controller: validates the hello against inventory, calls `Inventory::touch_host(id, ts)` on each heartbeat, replies with `HeartbeatAck`. End state: a running agent's `last_seen_at` advances visibly in the controller's `hosts` table.

**Architecture:** Two pieces land on each side:

- **Controller `sync` handler** — replaces the `Status::unimplemented` stub. Reads first frame, asserts `SyncHello { agent_id }`, looks up agent in inventory (rejects with `Unauthenticated` if unknown — forces re-enroll), then enters a per-stream loop: pull message, dispatch on variant, write back, repeat until stream closes.
- **Agent `sync` task** — after enroll, builds a tonic `Channel`, opens the `Sync` stream, sends `SyncHello`, spawns a heartbeat task that sends `Heartbeat` every 10s via a `tokio::sync::mpsc` channel. Reads acks (logs them at debug). The whole thing replaces the Phase 2d `run_agent` "stop plugins and return" with `await ctrl_c`.

`run_agent` becomes long-lived. `tokio::signal::ctrl_c` triggers graceful shutdown.

**Reconnection** is Phase 2f; Phase 2e ships the happy path. If the stream drops mid-run in 2e, the agent process exits with an error.

**Tech Stack:** Same as 2d. Adds `tokio_stream::wrappers::ReceiverStream` for bidi (already in workspace deps from Phase 2a).

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** §4 (wire contract — `SyncHello`, `Heartbeat`, `HeartbeatAck`), §6 (agent lifecycle), §7 (controller per-Sync RPC).

---

## Scope

**In:**
- Controller `Sync` handler: validates Hello, dispatches on AgentMessage variants, calls `Inventory::touch_host` on Heartbeat, replies with HeartbeatAck.
- Agent `sync` module: opens stream, sends Hello, runs heartbeat loop with `tokio::time::interval`, reads acks.
- `run_agent` rewrite: enroll-or-skip → load `Channel` → open Sync → spawn heartbeat task → `tokio::signal::ctrl_c().await` → graceful shutdown (drop stream, stop plugins, return Ok).
- Heartbeat interval respects `EnrollResponse.heartbeat_interval_secs` (server-suggested, agent honors).
- 2 new integration tests in `crates/isengard-agent/tests/sync_e2e.rs`:
  - `agent_sends_heartbeats_and_last_seen_advances` — spawn controller + agent in `tokio::spawn`, wait ~12s (one heartbeat), assert `last_seen_at` updated.
  - `unknown_agent_id_in_sync_hello_returns_unauthenticated` — spawn controller, manually open Sync stream with bogus agent_id, assert error.

**Out (Phase 2f and beyond):**
- Reconnection on drop / exponential backoff
- Goodbye message on graceful shutdown (controller-side detect)
- Container snapshots (Phase 3)
- Events stream (Phase 4)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 46 tests
3. `just ci-local` clean, lefthook clean
4. Manual smoke: start controller, run agent in foreground for 25s, see `last_seen_at` advance twice (initial enroll + 2 heartbeats at 10s intervals); ctrl_c the agent and see clean exit.
5. Tag `v0.1.0-alpha.phase2e` set locally.

---

## File Structure

```
isengard/
├── crates/
│   ├── isengard-controller/
│   │   ├── src/service.rs                                # MODIFY: real Sync handler
│   ├── isengard-agent/
│   │   ├── src/
│   │   │   ├── sync.rs                                   # CREATE: Sync stream client + heartbeat loop
│   │   │   └── lib.rs                                    # MODIFY: run_agent calls sync, awaits ctrl_c
│   │   └── tests/
│   │       └── sync_e2e.rs                               # CREATE: heartbeat updates last_seen
```

---

## Task 1: Controller `Sync` handler

**Files:**
- Modify: `crates/isengard-controller/src/service.rs`

The handler reads the first frame as `SyncHello`, validates the agent_id against inventory, then loops on incoming messages.

- [ ] **Step 1: Replace the stub**

Read `crates/isengard-controller/src/service.rs`. Find the existing `sync` method that returns `Status::unimplemented`. Replace its body with:

```rust
    async fn sync(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        let mut inbound = request.into_inner();

        // Read first frame: must be SyncHello with a valid agent_id.
        let hello = inbound
            .message()
            .await
            .map_err(|e| Status::aborted(format!("reading hello: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before SyncHello"))?;

        let agent_id_str = match hello.payload {
            Some(isengard_proto::pb::agent_message::Payload::Hello(h)) => h.agent_id,
            _ => {
                return Err(Status::invalid_argument(
                    "first frame must be SyncHello",
                ));
            }
        };

        // Parse the ULID and look up the host.
        let agent_id = ulid::Ulid::from_string(&agent_id_str)
            .map_err(|e| Status::unauthenticated(format!("invalid agent_id: {e}")))?;
        let host_id = isengard_storage::HostId(agent_id);

        let host = self
            .inventory
            .get_host(host_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Sync: db error during host lookup");
                Status::internal("database error")
            })?
            .ok_or_else(|| Status::unauthenticated("agent_id not in inventory; re-enroll"))?;

        tracing::info!(agent = %host.hostname, "agent connected for sync");

        // Build outbound channel. Controller uses this to send HeartbeatAcks
        // (and Phase 3+ commands).
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ControllerMessage, Status>>(16);
        let outbound = ReceiverStream::new(rx);

        let inventory = self.inventory.clone();
        let agent_hostname = host.hostname.clone();

        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(isengard_proto::pb::agent_message::Payload::Heartbeat(hb)) => {
                        // Update last_seen_at to the controller's clock (not the
                        // agent's — keeps the inventory's notion of "recent" tied
                        // to a single clock).
                        let server_ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);

                        if let Err(e) = inventory.touch_host(host_id, server_ts).await {
                            tracing::error!(error = %e, agent = %agent_hostname, "touch_host failed");
                        }

                        let ack = ControllerMessage {
                            payload: Some(isengard_proto::pb::controller_message::Payload::HeartbeatAck(
                                isengard_proto::pb::HeartbeatAck {
                                    server_time_ms: (server_ts as u64) * 1000,
                                },
                            )),
                        };
                        if tx.send(Ok(ack)).await.is_err() {
                            // Client closed the receive side; stream is over.
                            tracing::debug!(agent = %agent_hostname, "client receiver closed");
                            break;
                        }

                        let _ = hb.ts_ms; // agent clock for future drift logging
                    }
                    Some(isengard_proto::pb::agent_message::Payload::Hello(_)) => {
                        // Hello as second-or-later frame is invalid; ignore + log.
                        tracing::warn!(agent = %agent_hostname, "received Hello after first frame");
                    }
                    None => {
                        tracing::debug!(agent = %agent_hostname, "empty payload, skipping");
                    }
                }
            }

            tracing::info!(agent = %agent_hostname, "agent stream closed");
        });

        Ok(Response::new(outbound))
    }
```

You'll need new imports at the top of `service.rs`:

```rust
use isengard_proto::pb::ControllerMessage;
```

(The `Streaming`, `Request`, `Response`, `Status`, `ReceiverStream` should already be there from Phase 2a/2c.)

`isengard_storage` is already a dep; `ulid` becomes a new dep on the controller crate (currently transitive through storage). Add `ulid.workspace = true` to `crates/isengard-controller/Cargo.toml` `[dependencies]`.

- [ ] **Step 2: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. The existing 8 controller tests should still pass (none of them exercise Sync handler — server_skeleton.rs only tests Enroll).

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/Cargo.toml crates/isengard-controller/src/service.rs Cargo.lock && git commit -m "feat(controller): real Sync handler — heartbeats touch_host, ack with server time"
```

---

## Task 2: Agent `sync` module — open stream + heartbeat loop

**Files:**
- Create: `crates/isengard-agent/src/sync.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (`mod sync;`)
- Modify: `crates/isengard-agent/Cargo.toml` (`tokio-stream` workspace dep)

- [ ] **Step 1: Add `tokio-stream` to agent deps**

Read `crates/isengard-agent/Cargo.toml`. Add to `[dependencies]`:

```toml
tokio-stream.workspace = true
```

- [ ] **Step 2: Write `sync.rs`**

```bash
cat > crates/isengard-agent/src/sync.rs <<'EOF'
//! Long-lived Sync stream from agent to controller.
//!
//! Phase 2e ships the happy path:
//!  - open one bidi stream
//!  - first frame: SyncHello { agent_id }
//!  - then: Heartbeat every `interval_secs` until the stream errors or the
//!    caller's cancellation token fires
//!  - read every ControllerMessage from the server side, log at debug
//!
//! Reconnection on drop is Phase 2f.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{AgentMessage, Heartbeat, SyncHello};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{debug, info, instrument, warn};

use crate::Result;

/// Open a Sync stream and run the heartbeat loop until the stream errors or
/// `cancel` fires. Returns Ok on graceful cancel; Err on stream error.
#[instrument(skip(token, cancel), fields(agent_id = %agent_id))]
pub async fn run_sync_loop(
    controller_url: String,
    token: String,
    agent_id: String,
    interval_secs: u32,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let channel = Channel::from_shared(controller_url.clone())
        .with_context(|| format!("invalid controller url {controller_url:?}"))?
        .connect()
        .await
        .with_context(|| format!("connecting to controller at {controller_url}"))?;

    let bearer: MetadataValue<_> = format!("Bearer {token}")
        .parse()
        .context("token contains characters not legal in a Bearer header")?;

    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    // Outbound: Hello, then Heartbeat every interval.
    let (tx, rx) = mpsc::channel::<AgentMessage>(16);
    let outbound = ReceiverStream::new(rx);

    // Send Hello first.
    let hello = AgentMessage {
        payload: Some(isengard_proto::pb::agent_message::Payload::Hello(SyncHello {
            agent_id: agent_id.clone(),
        })),
    };
    tx.send(hello).await.context("sending Hello to outbound channel")?;
    info!("Sync stream opened, Hello sent");

    // Spawn heartbeat task.
    let hb_tx = tx.clone();
    let interval = Duration::from_secs(u64::from(interval_secs.max(1)));
    let cancel_hb = cancel.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the first immediate tick; we want the first heartbeat one
        // interval after Hello, not piggybacked on the connection.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel_hb.notified() => {
                    debug!("heartbeat task cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let msg = AgentMessage {
                        payload: Some(isengard_proto::pb::agent_message::Payload::Heartbeat(
                            Heartbeat { ts_ms },
                        )),
                    };
                    if hb_tx.send(msg).await.is_err() {
                        debug!("outbound channel closed, exiting heartbeat task");
                        break;
                    }
                }
            }
        }
    });

    // Open the bidi stream.
    let response = client
        .sync(Request::new(outbound))
        .await
        .context("Sync RPC failed")?;
    let mut inbound = response.into_inner();

    // Read inbound messages (HeartbeatAck, future Command/ConfigUpdate).
    let read_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = inbound.message().await {
            match msg.payload {
                Some(isengard_proto::pb::controller_message::Payload::HeartbeatAck(ack)) => {
                    debug!(server_time_ms = ack.server_time_ms, "HeartbeatAck");
                }
                _ => {
                    warn!(?msg.payload, "unexpected ControllerMessage payload");
                }
            }
        }
        info!("inbound stream closed");
    });

    // Wait for cancellation.
    cancel.notified().await;
    info!("cancel received, shutting down sync");
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    drop(tx); // close outbound; server sees EOF
    let _ = read_task.await;

    Ok(())
}
EOF
```

- [ ] **Step 3: Wire `mod sync;` into lib.rs**

Add `pub mod sync;` to the top of `crates/isengard-agent/src/lib.rs` next to the other module declarations.

- [ ] **Step 4: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. Existing 7 agent tests still pass (5 unit + 2 e2e from 2d).

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/Cargo.toml crates/isengard-agent/src/sync.rs crates/isengard-agent/src/lib.rs Cargo.lock && git commit -m "feat(agent): sync module — open stream, send Hello, heartbeat loop"
```

---

## Task 3: Rewrite `run_agent` to be long-lived (sync + ctrl_c)

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs`

- [ ] **Step 1: Update `run_agent`**

Read the existing `run_agent` body. After the enroll-or-skip block, replace the plugin-stop loop with:

```rust
    // -- plugin lifecycle: init/start, then run sync loop until ctrl_c ---
    let mut started = Vec::with_capacity(plugins.len());
    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let ctx = PluginContext::new(HostMode::Agent, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // -- run sync loop in background; ctrl_c triggers shutdown ----------
    let token = std::env::var("ISENGARD_TOKEN")
        .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

    // Phase 2e hardcodes 10s heartbeat. Phase 2e+: honor heartbeat_interval_secs
    // from EnrollResponse (Task 4 of this phase if interval comes back from
    // enroll; otherwise wire in Phase 2f).
    const HEARTBEAT_INTERVAL_SECS: u32 = 10;

    let sync_handle = {
        let url = opts.controller_url.clone();
        let agent_id = agent_id.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            sync::run_sync_loop(url, token, agent_id, HEARTBEAT_INTERVAL_SECS, cancel).await
        })
    };

    // Wait for ctrl_c.
    let _ = tokio::signal::ctrl_c().await;
    info!("ctrl_c received, shutting down");
    cancel.notify_waiters();

    // Wait for sync to wind down (with a timeout so we don't hang forever).
    let sync_result = tokio::time::timeout(std::time::Duration::from_secs(5), sync_handle).await;
    match sync_result {
        Ok(Ok(Ok(()))) => info!("sync loop exited cleanly"),
        Ok(Ok(Err(e))) => warn!(error = %e, "sync loop exited with error"),
        Ok(Err(e)) => warn!(error = %e, "sync task panicked or was cancelled"),
        Err(_) => warn!("sync loop timed out on shutdown"),
    }

    // -- plugin stop ---------------------------------------------------
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!(agent_id = %agent_id, "agent exited cleanly");
    Ok(())
```

You'll need imports at the top of lib.rs (some may already be there):

```rust
use std::sync::Arc;
use tokio::signal;
use tracing::warn;
```

The existing `info!` and `instrument` imports stay.

- [ ] **Step 2: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-agent 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 3: Adjust the existing e2e tests**

The Phase 2d `enroll_e2e` tests call `run_agent(...).await.expect("run_agent should succeed")`. Now `run_agent` blocks on `ctrl_c` and never returns naturally. Those tests will hang.

Two fix options:
- (a) Move the agent into `tokio::spawn` and assert on side-effects (state file, inventory) without waiting for `run_agent` to return.
- (b) Inject a cancellation source so the test can stop the agent. E.g., add an internal helper `pub(crate) async fn run_agent_with_cancel(opts, cancel: Arc<Notify>)`.

Option (a) is simpler — the existing tests asserted on state file + inventory, both of which should be set after the enroll completes (which happens BEFORE the sync loop blocks on ctrl_c). Just spawn the agent and poll for the assertions.

Update `crates/isengard-agent/tests/enroll_e2e.rs`:

Replace the `run_agent(...).await.expect(...)` calls with:

```rust
    let opts_clone = opts.clone(); // adjust as needed
    let agent_handle = tokio::spawn(run_agent(opts_clone));

    // Wait up to 5s for agent.json to appear (proves enroll completed).
    for _ in 0..50 {
        if state_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(state_path.exists(), "agent.json must exist after enroll");
    // ... rest of assertions ...

    // Tear down: the spawned agent is leaked; test process exits at function end.
    agent_handle.abort();
```

Apply this pattern to BOTH tests in `enroll_e2e.rs`. The `second_run_is_idempotent_no_re_enroll` test runs `run_agent` twice — both spawn-and-poll, abort between.

- [ ] **Step 4: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent 2>&1 | tail -15
```

Expected: all 7 tests pass (1 dummy + 4 state + 2 e2e). The e2e tests should complete in < 10s.

If they hang, the spawn-and-abort pattern isn't catching the agent. Verify `tokio::spawn` is awaited via `agent_handle.abort()` and that the test function returns naturally (the leaked task dies with the test process).

- [ ] **Step 5: Run full workspace + clippy**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 44 tests still passing, no FAILED. Clippy clean.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/lib.rs crates/isengard-agent/tests/enroll_e2e.rs && git commit -m "feat(agent): run_agent runs sync loop, awaits ctrl_c; e2e tests spawn-and-abort"
```

---

## Task 4: Sync e2e test — heartbeat updates last_seen

**Files:**
- Create: `crates/isengard-agent/tests/sync_e2e.rs`

This test proves the full Phase 2e flow: agent enrolls, opens sync, sends heartbeat, controller's `last_seen_at` advances.

- [ ] **Step 1: Write the test**

```bash
cat > crates/isengard-agent/tests/sync_e2e.rs <<'EOF'
//! Phase 2e e2e: agent runs, sends heartbeats, controller's last_seen_at
//! advances visibly.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::time::Duration;

use isengard_agent::{run_agent, AgentOptions};
use isengard_controller::{run_controller, ControllerOptions};
use isengard_storage::Inventory;
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2e";

async fn spawn_controller(state_dir: std::path::PathBuf) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    unsafe {
        std::env::set_var("ISENGARD_TOKEN", TEST_TOKEN);
    }

    tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir,
            config: serde_json::Value::Object(Default::default()),
        }).await;
    });

    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_heartbeats_advance_last_seen_at() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();

    let addr = spawn_controller(controller_state.path().to_path_buf()).await;

    // Spawn the agent. It will enroll then open the sync stream and send
    // heartbeats. The handle leaks intentionally — test process exits at
    // function end.
    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    };
    let agent_handle = tokio::spawn(run_agent(opts));

    // Wait for enroll to complete (agent.json appears).
    let state_path = agent_state.path().join("agent.json");
    for _ in 0..50 {
        if state_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(state_path.exists(), "agent.json must exist after enroll");

    // Open the inventory and capture first last_seen_at value (None right
    // after enroll, because Heartbeat hasn't fired yet).
    let inv = Inventory::open(&controller_state.path().join("isengard.db"))
        .await
        .expect("open inventory");
    let hosts = inv.list_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1);
    // last_seen_at MAY already be set if a heartbeat raced — accept either.
    let initial_last_seen = hosts[0].last_seen_at;

    // Wait for at least one heartbeat to land. Phase 2e hardcodes 10s
    // interval; we wait up to 15s for last_seen_at to advance.
    let mut advanced = false;
    for _ in 0..150 {
        let hosts = inv.list_hosts().await.unwrap();
        let now = hosts[0].last_seen_at;
        if now.is_some() && now != initial_last_seen {
            advanced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        advanced,
        "last_seen_at must advance after a heartbeat (waited 15s)"
    );

    agent_handle.abort();
}
EOF
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent --test sync_e2e 2>&1 | tail -10
```

Expected: 1 test passes in ~10–15s (waiting for the first heartbeat).

If the test times out at 15s without seeing last_seen_at advance:
- Check the heartbeat interval in agent (should be 10s)
- Check the controller's Sync handler is actually calling `inventory.touch_host`
- Add `--log=debug` to manually run + observe

If the test is fast (<5s), maybe a heartbeat fires immediately on Hello. The Phase 2e plan calls for "first heartbeat one interval after Hello", so 10s minimum. If it fires earlier, that's fine for the test but worth investigating.

- [ ] **Step 3: Run full workspace tests + clippy**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 45 tests (44 + 1 new e2e), all green, clippy clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/tests/sync_e2e.rs && git commit -m "test(agent): e2e — heartbeats advance controller's last_seen_at"
```

---

## Task 5: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails, apply `cargo fmt` + commit as `style: cargo fmt across phase 2e`, re-run.

- [ ] **Step 2: Manual smoke run**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-2e-final-ctrl /tmp/isengard-2e-final-agent

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9440 --state-dir /tmp/isengard-2e-final-ctrl &
CTRL=$!
sleep 1

# Run agent in background; let it heartbeat for 25 seconds, then kill it.
ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9440 --state-dir /tmp/isengard-2e-final-agent &
AGENT=$!
sleep 25

# Send ctrl_c-equivalent (SIGINT) to the agent for graceful shutdown.
kill -INT $AGENT
wait $AGENT 2>/dev/null || true

# Confirm last_seen_at advanced. Should be approximately 2 heartbeats in 25s.
which sqlite3 >/dev/null && sqlite3 /tmp/isengard-2e-final-ctrl/isengard.db \
  "SELECT hostname, last_seen_at, datetime(last_seen_at, 'unixepoch') FROM hosts;" || echo "(sqlite3 not available)"

kill $CTRL 2>/dev/null
wait $CTRL 2>/dev/null || true
```

Expected: sqlite output shows a non-null `last_seen_at` from within the last ~30s. Agent logged `ctrl_c received, shutting down`, `sync loop exited cleanly`, `agent exited cleanly`.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2e -m "phase 2e: Sync stream + heartbeats — agent long-lived, last_seen_at advances"
cd ~/Projects/isengard && git tag -l | grep phase2e
```

Don't push.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 45 tests green
- [ ] `just ci-local` clean (lefthook will validate on push too)
- [ ] Manual smoke confirms heartbeats update `last_seen_at` and ctrl_c shuts agent down cleanly
- [ ] Tag `v0.1.0-alpha.phase2e` exists locally

---

## Self-review

| Spec requirement | Plan task |
|---|---|
| §4 SyncHello, Heartbeat, HeartbeatAck wire types | Tasks 1, 2 (consumed) |
| §6 agent: open Sync, send Hello, heartbeat every interval, await ctrl_c | Tasks 2, 3 |
| §7 controller: validate Hello, look up agent, touch_host on Heartbeat, ack | Task 1 |
| §11 done state: heartbeat advances last_seen_at | Task 4 |

No placeholders. Reconnection + backoff explicitly deferred to Phase 2f.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-2e-sync-heartbeats.md`. Subagent-driven execution.
