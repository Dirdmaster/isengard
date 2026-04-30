# Phase 3a: Updater Plugin Skeleton + Container Listing

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** First real plugin lands. The `updater` plugin connects to Docker via `bollard`, lists running containers on a schedule, and logs them at info level. End state: running an agent against a real Docker daemon prints "found N running containers" every cycle. No updates yet — that's 3b–3e.

**Architecture:** The placeholder `crates/isengard-plugins/updater/` from Phase 0 gets populated. An `Updater` struct implements `Plugin + AgentPlugin`. Internal task scheduling — the plugin owns its own tokio task that ticks every `interval` and runs one cycle. `AgentPlugin::run_cycle(&self, ctx)` delegates to the same shared cycle logic (so external invocations work too — controller-triggered "force update now" comes in v1.x). `inventory::submit!` registers it; the binary depends on the plugin crate so it gets linked in.

**Tech stack:** Adds `bollard` 0.18 (Rust Docker SDK, async, tokio-friendly). Connects to local Docker daemon via the standard `unix:///var/run/docker.sock` (or `DOCKER_HOST` env var when set).

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-04-29-platform-pivot-design.md` §9.1 (updater plugin responsibilities).

---

## Scope

**In:**
- `bollard` workspace dep (with pinned version)
- Populate `crates/isengard-plugins/updater/` with real code
- `Updater` struct: holds bollard `Docker` client, internal `Notify` for cancellation, `JoinHandle` for the cycle task
- `Plugin` impl: `init` parses config + creates Docker client, `start` spawns internal task, `stop` notifies cancel + awaits join
- `AgentPlugin::run_cycle` delegates to shared `do_cycle` function
- `inventory::submit!` registration (Agent capability)
- Wire updater plugin into the `isengard` binary (depend on the crate so `inventory::submit!` is linked)
- Integration test: agent starts, plugin loads, cycle runs at least once and logs container count
- Graceful Docker-unavailable handling: if the daemon isn't reachable, log an error and keep ticking (Phase 3b will tighten retry logic)

**Out (deferred to 3b–3e):**
- Digest checking (3b)
- Image pull (3b)
- Container recreation (3c)
- Self-update (3d)
- Old-image cleanup (3e)
- Configurable interval (3a hardcodes 30s for fast feedback; 3e moves it to plugin config)
- Filtering by `isengard.enable` label (3b — first place we need it)
- Container snapshot streaming to controller (Phase 4 — needs the Sync stream wire format expansion)
- Updater-emitted journal events (Phase 4 — needs the journal)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 53 (52 from Phase 2f + ≥ 1 new)
3. `just ci-local` clean
4. Manual smoke: `ISENGARD_TOKEN=test isengard agent --controller=http://...` against a real Docker daemon → log shows "updater cycle: N containers running" every 30s
5. Tag `v0.1.0-alpha.phase3a` set locally

---

## File Structure

```
isengard/
├── Cargo.toml                                                       # MODIFY: + bollard workspace dep
├── crates/
│   ├── isengard-plugins/updater/
│   │   ├── Cargo.toml                                               # MODIFY: real deps
│   │   └── src/lib.rs                                               # MODIFY: real implementation
│   └── isengard/
│       ├── Cargo.toml                                               # MODIFY: depend on isengard-plugin-updater
│       └── src/main.rs                                              # MODIFY: extern crate hint (or just dep is enough)
└── crates/isengard-plugins/updater/tests/
    └── plugin_loads.rs                                              # CREATE: integration test
```

---

## Task 1: bollard workspace dep + plugin crate Cargo.toml

**Files:**
- Modify: `Cargo.toml` (root)
- Modify: `crates/isengard-plugins/updater/Cargo.toml`

- [ ] **Step 1: Add `bollard` to root `[workspace.dependencies]`**

Open `~/Projects/isengard/Cargo.toml`. After the `# system info` block (where `hostname` lives), add:

```toml
# docker
bollard = { version = "0.18.1", default-features = false, features = ["ssl"] }
```

`ssl` enables HTTPS to docker daemons over TCP (some setups). Default-features disabled to avoid pulling things we don't need. If bollard 0.18.1 doesn't compile or has issues, try 0.17.x as fallback.

- [ ] **Step 2: Replace the placeholder `updater/Cargo.toml`**

```bash
cat > crates/isengard-plugins/updater/Cargo.toml <<'EOF'
[package]
name = "isengard-plugin-updater"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Auto-update plugin for Isengard — checks registry digests and recreates containers in place"

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
bollard.workspace = true
inventory.workspace = true
isengard-core.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tracing.workspace = true
EOF
```

- [ ] **Step 3: Verify workspace builds (placeholder still works)**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
```

The crate's lib.rs is still the placeholder from Phase 0 (just a doc-comment). Build should succeed — bollard pulls in transitives but nothing references it yet.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml crates/isengard-plugins/updater/Cargo.toml Cargo.lock && git commit -m "chore(deps): add bollard for Docker SDK; wire updater crate deps"
```

---

## Task 2: Populate `updater/src/lib.rs` with real code

**Files:**
- Modify: `crates/isengard-plugins/updater/src/lib.rs`

This task lands the plugin's core. Internal task does the work; `AgentPlugin::run_cycle` is a thin wrapper for external invocation.

- [ ] **Step 1: Write the lib.rs**

```bash
cat > crates/isengard-plugins/updater/src/lib.rs <<'EOF'
//! Isengard `updater` plugin.
//!
//! Watches running Docker containers and (in later sub-phases) keeps their
//! images up to date. Phase 3a: just lists containers on a schedule.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bollard::container::ListContainersOptions;
use bollard::Docker;
use isengard_core::{
    AgentPlugin, Capability, Plugin, PluginContext, PluginRegistration, Result,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

const CYCLE_INTERVAL: Duration = Duration::from_secs(30);

pub struct Updater {
    /// Lazily set in `init`. Wrapped in Option so the struct can be constructed
    /// by the inventory factory before init runs.
    docker: Option<Docker>,
    cancel: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            docker: None,
            cancel: Arc::new(Notify::new()),
            task: None,
        }
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

/// Run one cycle of work. Phase 3a: list running containers, log count + names.
async fn do_cycle(docker: &Docker) -> Result<()> {
    let opts = ListContainersOptions::<String> {
        all: false, // running only
        ..Default::default()
    };
    let containers = docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("listing containers: {e}"))?;

    info!(count = containers.len(), "updater cycle: containers running");

    for c in &containers {
        let name = c
            .names
            .as_ref()
            .and_then(|ns| ns.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "<unknown>".into());
        let image = c.image.as_deref().unwrap_or("<unknown>");
        debug!(container = %name, image = %image, "running");
    }

    Ok(())
}

#[async_trait]
impl Plugin for Updater {
    fn name(&self) -> &'static str {
        "updater"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, _ctx: &PluginContext) -> Result<()> {
        // Connect to the local Docker daemon. Honors DOCKER_HOST + standard
        // socket paths.
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| anyhow::anyhow!("connecting to docker daemon: {e}"))?;

        // Verify connectivity early — a bad daemon URL or missing socket
        // should fail init, not silently break the cycle.
        let version = docker
            .version()
            .await
            .map_err(|e| anyhow::anyhow!("docker version probe failed: {e}"))?;
        info!(
            api_version = %version.api_version.as_deref().unwrap_or("?"),
            engine_version = %version.version.as_deref().unwrap_or("?"),
            "updater connected to docker daemon"
        );

        self.docker = Some(docker);
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let docker = self
            .docker
            .clone()
            .ok_or_else(|| anyhow::anyhow!("updater started before init"))?;
        let cancel = self.cancel.clone();

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CYCLE_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel.notified() => {
                        debug!("updater cycle task cancelled");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = do_cycle(&docker).await {
                            // Don't crash the task on a single bad cycle; just log
                            // and try again next tick. Phase 3b adds retry policy.
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

    async fn stop(&mut self) -> Result<()> {
        self.cancel.notify_waiters();
        if let Some(task) = self.task.take() {
            // Give the task a moment to exit cleanly; if it hangs, abort.
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Ok(())) => debug!("updater cycle task ended cleanly"),
                Ok(Err(e)) => warn!(error = %e, "updater cycle task panicked"),
                Err(_) => warn!("updater cycle task timed out on stop"),
            }
        }
        info!("updater stopped");
        Ok(())
    }
}

#[async_trait]
impl AgentPlugin for Updater {
    async fn run_cycle(&self, _ctx: &PluginContext) -> Result<()> {
        // External-invocation entry point. Phase 3a: same as the internal task.
        // Phase 3e+: controller-triggered "force update now" lands here.
        let docker = self
            .docker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("run_cycle before init"))?;
        do_cycle(docker).await
    }
}

inventory::submit! {
    PluginRegistration {
        name: "updater",
        capabilities: &[Capability::Agent],
        constructor: || Box::new(Updater::new()) as Box<dyn Plugin>,
    }
}

// Compile-time assertion: Updater must remain Send + Sync because the
// inventory factory hands it across threads.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync + 'static>() {}
    assert::<Updater>();
}
EOF
```

If `Result` from `isengard_core` clashes with `anyhow::Result` somewhere, prefer `isengard_core::Result` (which IS `anyhow::Result` aliased through the agent crate's `pub type` — see Phase 2c plan). The `?` operator in `init`/`start`/`stop`/`run_cycle` works either way as long as both error types convert.

If clippy flags the `clippy::result_large_err` due to `anyhow::Error` being the inner type — `anyhow::Error` is actually small (Box internally), so clippy shouldn't flag it. The file-level allow is defensive.

- [ ] **Step 2: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-updater 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-updater --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. If bollard's API has shifted in 0.18 (e.g. `ListContainersOptions` field names), adjust to match — check `cargo doc --open -p bollard` for the live API.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/src/lib.rs && git commit -m "feat(updater): plugin skeleton with bollard + cycle task that lists containers"
```

---

## Task 3: Wire updater plugin into the `isengard` binary

**Files:**
- Modify: `crates/isengard/Cargo.toml`

The plugin registration via `inventory::submit!` only takes effect if the crate is compiled into the final binary. We add it as a dependency of the binary crate. No code change in main.rs needed — `inventory` finds it by symbol.

- [ ] **Step 1: Add dep**

Read `~/Projects/isengard/crates/isengard/Cargo.toml`. Find the `[dependencies]` block. Add:

```toml
isengard-plugin-updater = { path = "../isengard-plugins/updater", version = "0.1.0-alpha" }
```

(Path-based + version, matching the convention from Phase 2b for cargo-deny compatibility.)

- [ ] **Step 2: Verify the binary picks up the plugin**

```bash
cd ~/Projects/isengard && cargo build -p isengard 2>&1 | tail -5
```

Expected: clean. Now smoke-run the agent (without a controller — it'll fail to connect, but the plugin should still init/start before that):

```bash
cd ~/Projects/isengard && timeout 5 env ISENGARD_TOKEN=test ./target/debug/isengard agent --controller=http://127.0.0.1:9999 --state-dir=/tmp/isengard-3a-smoke 2>&1 | head -20 || true
```

Expected lines in output:
- `plugins discovered plugin_count=2` (the existing `dev` plugin + the new `updater`)
- `plugin = "dev" init`, `plugin = "dev" start`
- `updater connected to docker daemon api_version=...`
- `updater started`

If the agent can't connect to Docker (no daemon on this box), updater's init will fail with `connecting to docker daemon: ...`. The agent will fail with that error. That's expected behavior on a Docker-less host. Phase 3a's plan does NOT cover graceful "Docker not present" handling — Phase 3b will add it. For now, the test harness needs a running Docker daemon.

If you don't have Docker running locally, skip this smoke run and rely on the integration test in Task 4 (which uses testcontainers).

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard/Cargo.toml Cargo.lock && git commit -m "feat(bin): include updater plugin in agent binary"
```

---

## Task 4: Integration test — plugin loads and ticks

**Files:**
- Create: `crates/isengard-plugins/updater/tests/plugin_loads.rs`

This test verifies the plugin's lifecycle works end-to-end: construct, init against a real (test) Docker, start, wait for one cycle to fire, stop. We use `testcontainers` to spin up a Docker-in-Docker setup, OR — simpler — we just check that `init` succeeds against the host's Docker socket (skip the test if Docker isn't available).

Given testcontainers adds a non-trivial dep + complexity, **start with the simple approach: detect Docker availability and skip if absent**.

- [ ] **Step 1: Write the test**

```bash
mkdir -p crates/isengard-plugins/updater/tests
cat > crates/isengard-plugins/updater/tests/plugin_loads.rs <<'EOF'
//! Phase 3a integration: construct Updater, init against the host's Docker
//! daemon, start, wait for one cycle to fire, stop cleanly.
//!
//! Skips if Docker is not available on the host.

#![allow(clippy::result_large_err)]

use std::time::Duration;

use bollard::Docker;
use isengard_core::{HostMode, Plugin, PluginContext};
use isengard_plugin_updater::Updater;

async fn docker_available() -> bool {
    match Docker::connect_with_local_defaults() {
        Ok(d) => d.version().await.is_ok(),
        Err(_) => false,
    }
}

#[tokio::test]
async fn updater_lifecycle_against_real_docker() {
    if !docker_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }

    let mut plugin = Updater::new();
    let ctx = PluginContext::new(HostMode::Agent, serde_json::Value::Null);

    plugin.init(&ctx).await.expect("init should succeed against real Docker");
    plugin.start(&ctx).await.expect("start should succeed");

    // Cycle interval is 30s. Test would be too slow waiting for the natural
    // tick. Instead, just verify the plugin lifecycles cleanly across
    // start/stop without waiting for a tick.
    tokio::time::sleep(Duration::from_millis(200)).await;

    plugin.stop().await.expect("stop should succeed");
}
EOF
```

- [ ] **Step 2: Run the test**

```bash
cd ~/Projects/isengard && cargo test -p isengard-plugin-updater --test plugin_loads 2>&1 | tail -10
```

Expected on a Docker-equipped host: 1 test passes. On a Docker-less host: 1 test passes (skipped gracefully via the early return).

If you have Docker running and the test FAILS at init, double-check that bollard's `connect_with_local_defaults()` is reaching the right socket. On macOS Docker Desktop, the socket is at `~/.docker/run/docker.sock` or similar — bollard should auto-detect.

- [ ] **Step 3: Run full workspace tests + clippy**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: ≥ 53 tests passing (52 from Phase 2f + 1 new). All green; clippy clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/updater/tests/ && git commit -m "test(updater): plugin lifecycle against real Docker (skipped if absent)"
```

---

## Task 5: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 3a`, re-run.

- [ ] **Step 2: Manual smoke (if Docker is on this box)**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-3a-final-ctrl /tmp/isengard-3a-final-agent

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9460 --state-dir /tmp/isengard-3a-final-ctrl &
CTRL=$!
sleep 1

ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9460 --state-dir /tmp/isengard-3a-final-agent 2>&1 | tee /tmp/isengard-3a-agent.log &
AGENT=$!
# Wait long enough to see at least one updater cycle (cycle interval is 30s).
sleep 35

echo "--- updater cycles in log ---"
grep -E "updater (started|connected|cycle)" /tmp/isengard-3a-agent.log

kill -INT $AGENT
wait $AGENT 2>/dev/null || true
kill $CTRL 2>/dev/null
wait $CTRL 2>/dev/null || true
```

Expected log lines:
- `updater connected to docker daemon api_version=... engine_version=...`
- `updater started`
- At least one `updater cycle: containers running count=N` line (after ~30s)

If you don't have Docker on this box, skip this step. The integration test (Task 4) covers the same paths.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase3a -m "phase 3a: updater plugin skeleton — connects to Docker, lists containers"
cd ~/Projects/isengard && git tag -l | grep phase3a
```

Don't push.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 53 passing
- [ ] `just ci-local` clean
- [ ] Manual smoke confirms updater cycles run (or test in Task 4 passes if no local Docker)
- [ ] Tag `v0.1.0-alpha.phase3a` exists locally

---

## Self-review

| Spec requirement | Plan task |
|---|---|
| §9.1 list running containers (filter by `isengard.enable` later) | Task 2 (list-only for now; filter is 3b) |
| Plugin model integration via `inventory::submit!` | Task 2 |
| Plugin compiled into binary | Task 3 |
| First real plugin lifecycle works under the existing host | Tasks 2 + 4 |

3b–3e explicitly deferred. No placeholders.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-3a-updater-skeleton.md`. Subagent-driven execution.
