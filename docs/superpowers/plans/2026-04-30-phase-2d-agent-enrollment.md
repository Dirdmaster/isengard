# Phase 2d: Agent Client — Enroll + Persist `agent_id`

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The agent boots, reads its state file (if any), and either enrolls with the controller (first run) or skips enrollment (subsequent runs). After successful enrollment the controller-assigned `agent_id` is persisted to `<state_dir>/agent.json`. End state: a real agent process can be pointed at a real controller, and one row appears in the controller's `hosts` table.

**Architecture:** Three pieces land:
1. `--state-dir` flag on the agent subcommand. `AgentOptions` carries `controller_url: String` (required) + `state_dir: PathBuf`.
2. New module `agent_state` in `isengard-agent`: serde-clean `AgentState { agent_id: String }`, `load(path) -> Result<Option<AgentState>>`, `save(path, state) -> Result<()>`. Atomic write (temp + rename).
3. New `enroll` module: builds a tonic `Channel` to the controller, attaches the bearer-token interceptor, calls `Enroll`, returns the assigned `agent_id`. `run_agent` orchestrates: load state → if missing call enroll + save state → run plugin lifecycle → return. Sync stream + heartbeat loop is Phase 2e.

`run_agent` still calls `plugin.init/start/stop` and runs to completion (no long-lived loop yet — Phase 2e replaces this with the bidi stream + ctrl_c await).

**Tech Stack:** Same as 2a–2c. Adds `tonic` as an `isengard-agent` dep (was a test-only ref before), plus `tokio` filesystem ops which are already pulled in via tokio's `full` feature.

**Branch:** `next`. Local pre-push hook now active — every push runs `cargo fmt --check + clippy + tests + cargo-deny` first. Don't push without explicit approval.

**Spec:** §6 agent lifecycle, §13 sub-phase 2d.

---

## Scope

**In:**
- `--state-dir` flag on the agent subcommand (default: `/var/lib/isengard` for parity with controller; doc note that dev typically uses `~/.local/state/isengard`)
- `AgentOptions { controller_url, state_dir, config }` — `controller_url` is now required (was optional)
- `agent_state` module: `AgentState`, `load`, `save`, atomic writes
- `enroll` module: tonic client construction with bearer interceptor, `Enroll` call, returns assigned `agent_id`
- `run_agent` rewrite: load → enroll-if-missing → persist → plugin lifecycle → return
- Unit tests for state module (round-trip, missing-file, malformed-file)
- End-to-end integration test in `crates/isengard-agent/tests/enroll_e2e.rs`: spawn controller + run agent → assert state file + inventory row both populated

**Out (still deferred):**
- Sync stream + heartbeat loop (Phase 2e)
- Reconnection logic / exponential backoff (Phase 2f)
- mTLS (post-v1)
- `isengard-test-harness` crate (extract when 2e/2f reuse)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 43 passing tests (39 from Phase 2c + ≥ 4 new)
3. `just ci-local` passes
4. `lefthook` pre-push validates clean
5. Manual smoke: start controller in one terminal, run agent in another → agent prints "enrolled, agent_id=01J..." and exits cleanly. Re-run agent → prints "already enrolled, agent_id=01J..." and exits cleanly.
6. Tag `v0.1.0-alpha.phase2d` set locally
7. ~5 commits

---

## File Structure

```
isengard/
├── crates/
│   ├── isengard-agent/
│   │   ├── Cargo.toml                                    # MODIFY: + isengard-proto, tonic, tonic-types?, tempfile (dev-deps)
│   │   ├── src/
│   │   │   ├── lib.rs                                    # MODIFY: AgentOptions extended, run_agent rewritten, mod agent_state, mod enroll
│   │   │   ├── agent_state.rs                            # CREATE: AgentState + load/save (atomic)
│   │   │   └── enroll.rs                                 # CREATE: tonic client + interceptor + Enroll call
│   │   └── tests/
│   │       └── enroll_e2e.rs                             # CREATE: spawn controller + run agent + assert
│   └── isengard/
│       └── src/main.rs                                   # MODIFY: agent subcommand gets --state-dir, controller becomes required
```

---

## Task 1: `--state-dir` flag + extend `AgentOptions`

**Files:**
- Modify: `crates/isengard-agent/src/lib.rs`
- Modify: `crates/isengard/src/main.rs`

- [ ] **Step 1: Update `AgentOptions`**

Read `crates/isengard-agent/src/lib.rs`. Replace the existing `AgentOptions`:

```rust
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller, e.g. `http://controller.example.com:9417`.
    /// Required — Phase 2d removes the `Option` wrapper from Phase 1.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
}
```

Remove the `impl Default for AgentOptions { ... }` block — controller_url has no sensible default.

- [ ] **Step 2: Update binary `Agent` subcommand**

Read `crates/isengard/src/main.rs`. Find the `Command::Agent` variant in `enum Command`. Replace it with:

```rust
    Agent {
        /// URL of the controller, e.g. `http://controller.example.com:9417`.
        #[arg(long, env = "ISENGARD_CONTROLLER")]
        controller: String,
        /// Directory where the agent persists state (agent.json, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
    },
```

(Note: `controller` is no longer `Option<String>` — it's required.)

In the match arm, plumb both:

```rust
        Command::Agent { controller, state_dir } => {
            let _token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;

            std::fs::create_dir_all(&state_dir)
                .map_err(|e| anyhow::anyhow!("creating state dir {state_dir:?}: {e}"))?;

            tracing::info!(%controller, ?state_dir, "agent mode");
            isengard_agent::run_agent(isengard_agent::AgentOptions {
                controller_url: controller,
                state_dir,
                config: serde_json::Value::Object(Default::default()),
            }).await
        }
```

- [ ] **Step 3: Update `run_agent` body to USE `state_dir` (basic plumb-through; no enroll yet)**

In `crates/isengard-agent/src/lib.rs`, in the existing `run_agent`, change the log line to include `state_dir`:

```rust
info!(controller = %opts.controller_url, ?state_dir = ?opts.state_dir, "starting agent");
```

(Or just `info!(controller = %opts.controller_url, state_dir = ?opts.state_dir, "starting agent");` — preserve the field syntax that works with tracing.)

The plugin lifecycle stays the same — Phase 2d Task 3 replaces the body more substantially. For Task 1, just adjust the struct + log so the workspace still builds.

- [ ] **Step 4: Verify build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -3
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
```

Expected: clean, 39 tests still green (Phase 2c baseline). The existing agent integration test (`agent_mode_loads_dev_plugin`) currently invokes the binary without `--controller`, which now fails because the flag is required. **Update that test** to pass `--controller http://localhost:9999` (a dummy URL — the test currently checks plugin loading, not actual controller contact, so any URL is fine). The Phase 2d Task 5 integration test does the real end-to-end check.

If `agent_mode_loads_dev_plugin` test regresses, either:
- (preferred) add `.args(["--controller", "http://localhost:9999", ...])` to the existing assert_cmd invocation
- (alt) drop it entirely — Phase 2d Task 5's real e2e test obsoletes it; the controller_help/agent_help tests still cover CLI surface

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/src/lib.rs crates/isengard/src/main.rs crates/isengard/tests/plugin_loading.rs 2>/dev/null
cd ~/Projects/isengard && git commit -m "feat(agent): + state_dir flag, controller_url required, drop AgentOptions::Default"
```

---

## Task 2: `agent_state` module

**Files:**
- Create: `crates/isengard-agent/src/agent_state.rs`
- Modify: `crates/isengard-agent/src/lib.rs` (add `mod agent_state;`)
- Modify: `crates/isengard-agent/Cargo.toml` (already has serde, serde_json, tokio — verify)

- [ ] **Step 1: Write `agent_state.rs`**

```bash
cat > crates/isengard-agent/src/agent_state.rs <<'EOF'
//! Agent on-disk state: a single `agent.json` carrying the controller-assigned
//! `agent_id`. Survives agent restarts so the agent doesn't re-enroll.
//!
//! Atomic write: write to a `.tmp` file, then `rename` over the target.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;

const STATE_FILENAME: &str = "agent.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
}

/// Load the state file from `<state_dir>/agent.json`. Returns `Ok(None)` if the
/// file doesn't exist yet (first boot). Returns `Err` only on actual IO or
/// parse errors.
pub async fn load(state_dir: &Path) -> Result<Option<AgentState>> {
    let path = state_dir.join(STATE_FILENAME);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let state: AgentState = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parsing {path:?}: {e}"))?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {path:?}: {e}")),
    }
}

/// Atomically write the state file to `<state_dir>/agent.json`.
/// Strategy: serialize → write to `<file>.tmp` → fsync → rename over target.
pub async fn save(state_dir: &Path, state: &AgentState) -> Result<()> {
    let path = state_dir.join(STATE_FILENAME);
    let tmp = state_dir.join(format!("{STATE_FILENAME}.tmp"));

    let body = serde_json::to_vec_pretty(state)
        .map_err(|e| anyhow::anyhow!("serialising state: {e}"))?;

    tokio::fs::write(&tmp, &body).await
        .map_err(|e| anyhow::anyhow!("writing {tmp:?}: {e}"))?;
    tokio::fs::rename(&tmp, &path).await
        .map_err(|e| anyhow::anyhow!("rename {tmp:?} -> {path:?}: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn load_returns_none_for_empty_dir() {
        let dir = TempDir::new().unwrap();
        let result = load(dir.path()).await.expect("load should not error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let original = AgentState {
            agent_id: "01J7XYZAGENTID000000000000".into(),
        };

        save(dir.path(), &original).await.expect("save");
        let loaded = load(dir.path()).await.expect("load").expect("Some");
        assert_eq!(loaded, original);
    }

    #[tokio::test]
    async fn load_returns_error_on_malformed_file() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("agent.json"), b"{not valid json").await.unwrap();

        let err = load(dir.path()).await.expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains("parsing") || msg.contains("agent.json"), "got: {msg}");
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let first = AgentState { agent_id: "first".into() };
        let second = AgentState { agent_id: "second".into() };
        save(dir.path(), &first).await.unwrap();
        save(dir.path(), &second).await.unwrap();
        let loaded = load(dir.path()).await.unwrap().unwrap();
        assert_eq!(loaded, second);
    }
}
EOF
```

- [ ] **Step 2: Add `tempfile` to agent dev-deps**

Read `crates/isengard-agent/Cargo.toml`. Add (or extend) `[dev-dependencies]`:

```toml
[dev-dependencies]
tempfile = "3.14.0"
```

(If `[dev-dependencies]` already exists, just add the `tempfile` line. The version matches what's in `isengard-storage` and `isengard-controller`.)

- [ ] **Step 3: Wire `mod agent_state;` into lib.rs**

In `crates/isengard-agent/src/lib.rs`, add to the top:

```rust
pub mod agent_state;
```

Also need a `Result` alias for the module to use. Add to lib.rs (if not already there):

```rust
pub use anyhow::Result;
```

Or define your own:

```rust
pub type Result<T> = anyhow::Result<T>;
```

The `agent_state.rs` code references `crate::Result` — make sure it resolves.

- [ ] **Step 4: Run tests + clippy**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-agent --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 5 tests pass (1 existing dummy + 4 new state tests). Clippy clean.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/Cargo.toml crates/isengard-agent/src/agent_state.rs crates/isengard-agent/src/lib.rs Cargo.lock 2>/dev/null
cd ~/Projects/isengard && git commit -m "feat(agent): agent_state module — atomic save/load of agent.json"
```

---

## Task 3: `enroll` module + rewrite `run_agent`

**Files:**
- Create: `crates/isengard-agent/src/enroll.rs`
- Modify: `crates/isengard-agent/Cargo.toml` (add `isengard-proto`, `tonic`)
- Modify: `crates/isengard-agent/src/lib.rs` (add `mod enroll;`, rewrite `run_agent`)

- [ ] **Step 1: Add `isengard-proto` + `tonic` to agent deps**

Read `crates/isengard-agent/Cargo.toml`. Add to `[dependencies]`:

```toml
isengard-proto.workspace = true
tonic.workspace = true
```

- [ ] **Step 2: Write `enroll.rs`**

```bash
cat > crates/isengard-agent/src/enroll.rs <<'EOF'
//! Agent → controller enrollment.
//!
//! Builds a tonic Channel, attaches the bearer-token interceptor, calls the
//! `Enroll` RPC once, and returns the controller-assigned `agent_id`.

use anyhow::Context;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::EnrollRequest;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use crate::Result;

/// Resolved host metadata included in the EnrollRequest.
pub struct HostInfo {
    pub fingerprint: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub agent_version: String,
    pub docker_version: String,
}

impl HostInfo {
    /// Best-effort: hostname from the OS, OS/arch from rustc consts, agent
    /// version from cargo, docker version blank for now (Phase 3 queries
    /// the docker daemon).
    pub fn detect() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "unknown-host".into());

        Self {
            fingerprint: hostname.clone(), // Phase 2d uses hostname as fingerprint; machine-id later
            hostname,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version: String::new(), // Phase 3
        }
    }
}

/// Issue an Enroll RPC against the configured controller. Returns the
/// controller-assigned `agent_id` as a string.
pub async fn enroll(controller_url: &str, token: &str, info: HostInfo) -> Result<String> {
    let channel = Channel::from_shared(controller_url.to_string())
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

    let req = EnrollRequest {
        fingerprint: info.fingerprint,
        hostname: info.hostname,
        os: info.os,
        arch: info.arch,
        agent_version: info.agent_version,
        docker_version: info.docker_version,
    };

    let resp = client
        .enroll(req)
        .await
        .context("Enroll RPC failed")?
        .into_inner();

    if resp.agent_id.is_empty() {
        anyhow::bail!("controller returned empty agent_id");
    }

    Ok(resp.agent_id)
}
EOF
```

- [ ] **Step 3: Add `hostname` crate to workspace + agent deps**

`hostname` is a tiny crate that resolves the local hostname cross-platform. Add to root `Cargo.toml` workspace.dependencies:

```toml
# system info
hostname = "0.4.0"
```

(Place it after `# crypto helpers` or somewhere logical.)

Then in `crates/isengard-agent/Cargo.toml`, add `hostname.workspace = true` to deps.

- [ ] **Step 4: Rewrite `run_agent`**

Open `crates/isengard-agent/src/lib.rs`. Replace the existing `run_agent` body with:

```rust
#[instrument(skip(opts), fields(controller = %opts.controller_url))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(state_dir = ?opts.state_dir, "starting agent");

    // -- determine agent_id ----------------------------------------------
    let existing = agent_state::load(&opts.state_dir).await?;
    let agent_id = match existing {
        Some(state) => {
            info!(agent_id = %state.agent_id, "already enrolled, skipping enroll");
            state.agent_id
        }
        None => {
            info!("no agent.json found, enrolling with controller");
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let host_info = enroll::HostInfo::detect();
            let id = enroll::enroll(&opts.controller_url, &token, host_info).await?;
            agent_state::save(
                &opts.state_dir,
                &agent_state::AgentState { agent_id: id.clone() },
            )
            .await?;
            info!(agent_id = %id, "enrolled");
            id
        }
    };

    // -- plugin lifecycle (unchanged from Phase 1) -----------------------
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

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

    // Phase 2d: stop every plugin and return. Phase 2e holds the runner open
    // on a Sync stream + ctrl_c await.
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!(agent_id = %agent_id, "agent exited cleanly");
    let _ = agent_id; // silence unused if no log line references it after Phase 2e
    Ok(())
}
```

Add `mod agent_state;` and `mod enroll;` to lib.rs's top if not already there:

```rust
pub mod agent_state;
pub mod enroll;
```

- [ ] **Step 5: Verify build + clippy + tests**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cd ~/Projects/isengard && cargo test -p isengard-agent 2>&1 | tail -10
```

Expected: clean. 5 tests pass (1 existing + 4 state).

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git status --short
cd ~/Projects/isengard && git add crates/isengard-agent/Cargo.toml crates/isengard-agent/src/lib.rs crates/isengard-agent/src/enroll.rs Cargo.toml Cargo.lock 2>/dev/null
cd ~/Projects/isengard && git commit -m "feat(agent): enroll module + run_agent does enroll-or-skip with state persistence"
```

---

## Task 4: End-to-end integration test

**Files:**
- Create: `crates/isengard-agent/tests/enroll_e2e.rs`

This test starts a real controller in-process, runs the agent against it, and verifies both sides: agent's `agent.json` exists, controller's inventory has a row.

- [ ] **Step 1: Write the test**

```bash
cat > crates/isengard-agent/tests/enroll_e2e.rs <<'EOF'
//! End-to-end Phase 2d test: spawn a real controller, run the agent, verify
//! enrollment lands on both sides (agent.json exists, controller inventory
//! has a row).

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::time::Duration;

use isengard_agent::{run_agent, AgentOptions};
use isengard_controller::{run_controller, ControllerOptions};
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2d";

async fn spawn_controller(state_dir: std::path::PathBuf) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // SAFETY: env::set_var is unsafe in Rust 2024. Set the token once before
    // any concurrent access; both controller and agent read this same env var.
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

#[tokio::test]
async fn agent_enrolls_writes_state_and_appears_in_inventory() {
    // Two separate temp dirs: controller has its own, agent has its own.
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();

    let addr = spawn_controller(controller_state.path().to_path_buf()).await;

    // Run the agent.
    run_agent(AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    })
    .await
    .expect("run_agent should succeed");

    // Agent side: agent.json must exist with a non-empty agent_id.
    let state_path = agent_state.path().join("agent.json");
    assert!(state_path.exists(), "agent.json must exist after enroll");
    let body = std::fs::read_to_string(&state_path).unwrap();
    assert!(body.contains("agent_id"), "agent.json must contain agent_id key");

    // Controller side: hosts table must contain one row.
    use isengard_storage::Inventory;
    let db_path = controller_state.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.expect("open inventory");
    let hosts = inv.list_hosts().await.expect("list hosts");
    assert_eq!(hosts.len(), 1, "controller inventory should have exactly 1 host");
    assert!(!hosts[0].fingerprint.is_empty(), "fingerprint should be set");
}

#[tokio::test]
async fn second_run_is_idempotent_no_re_enroll() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();
    let addr = spawn_controller(controller_state.path().to_path_buf()).await;

    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    };

    // First run: enrolls.
    run_agent(opts.clone()).await.expect("first run");

    // Read the agent_id written by the first run.
    let body1 = std::fs::read_to_string(agent_state.path().join("agent.json")).unwrap();

    // Second run: must NOT re-enroll. agent.json should be unchanged.
    run_agent(opts).await.expect("second run");
    let body2 = std::fs::read_to_string(agent_state.path().join("agent.json")).unwrap();
    assert_eq!(body1, body2, "second run should not change agent.json");

    // Controller still has exactly 1 host.
    use isengard_storage::Inventory;
    let inv = Inventory::open(&controller_state.path().join("isengard.db"))
        .await
        .unwrap();
    let hosts = inv.list_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1, "should still be 1 host after second run");
}
EOF
```

- [ ] **Step 2: Add `isengard-controller` and `isengard-storage` to agent dev-deps**

Read `crates/isengard-agent/Cargo.toml`. Add to `[dev-dependencies]`:

```toml
isengard-controller = { workspace = true }
isengard-storage = { workspace = true }
```

(`tempfile` should already be there from Task 2.)

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-agent --test enroll_e2e 2>&1 | tail -10
```

Expected: 2 tests pass (`agent_enrolls_writes_state_and_appears_in_inventory`, `second_run_is_idempotent_no_re_enroll`).

If either test hangs (most likely: `run_controller` waiting on ctrl_c blocks the test process), check that the controller is spawned in a `tokio::spawn` that's allowed to leak. The test process exits as soon as the test function returns; spawned tasks die with it.

If `run_agent` hangs trying to connect, the controller may not have bound in time — increase the bind-poll loop iterations from 40 to 80.

If `run_agent` returns "ISENGARD_TOKEN env var must be set" but the harness sets it, that's an ordering issue — `unsafe { set_var }` MUST happen before the first `run_*` call.

- [ ] **Step 4: Run full workspace tests + clippy**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: ≥ 43 tests, all green; clippy clean.

- [ ] **Step 5: Smoke run end-to-end (optional but recommended)**

```bash
# Terminal 1: controller
mkdir -p /tmp/isengard-2d-ctrl
ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9430 --state-dir /tmp/isengard-2d-ctrl &
CTRL=$!
sleep 1

# Terminal 2 (same shell): agent
mkdir -p /tmp/isengard-2d-agent
ISENGARD_TOKEN=test ./target/debug/isengard agent --controller http://127.0.0.1:9430 --state-dir /tmp/isengard-2d-agent
cat /tmp/isengard-2d-agent/agent.json
sqlite3 /tmp/isengard-2d-ctrl/isengard.db "SELECT hostname, agent_version FROM hosts;" 2>/dev/null || echo "(sqlite3 cli not available; skip)"

kill $CTRL 2>/dev/null
```

Expected: agent prints "enrolled, agent_id=01J...". `agent.json` contains the same id. Controller's hosts table has one row.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-agent/Cargo.toml crates/isengard-agent/tests/ Cargo.lock 2>/dev/null
cd ~/Projects/isengard && git commit -m "test(agent): e2e — agent enrolls, persists state, controller sees host"
```

---

## Task 5: CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 2d`, re-run.

The lefthook pre-push hook will also run this on the actual push.

- [ ] **Step 2: Verify test count**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
```

Expected: ≥ 43 tests.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2d -m "phase 2d: agent enrolls, persists agent_id, controller sees host"
cd ~/Projects/isengard && git tag -l | grep phase2d
```

Don't push (tag or branch) without explicit approval.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 43 green
- [ ] `just ci-local` clean (and lefthook would clear too)
- [ ] Manual smoke confirms agent → controller enrollment works
- [ ] Tag `v0.1.0-alpha.phase2d` exists locally
- [ ] No pushes since the plan commit

---

## Self-review

| Spec requirement | Plan task |
|---|---|
| §6 agent loads $ISENGARD_TOKEN, controller URL, state file | Task 1, 3 |
| §6 enroll-or-skip based on stored agent_id | Task 3 |
| §6 atomic save of agent_id | Task 2 |
| §13 sub-phase 2d: agent contacts controller, agent_id appears on disk and in inventory | Task 4 (e2e test asserts both) |

Phase 2e (sync stream + heartbeats), 2f (reconnection), 2g (test harness extraction) explicitly deferred. No placeholders in the plan.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-2d-agent-enrollment.md`.

Subagent-driven execution, same pattern as 2a/2b/2c.
