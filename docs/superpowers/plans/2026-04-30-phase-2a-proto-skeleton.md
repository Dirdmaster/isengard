# Phase 2a: Proto Schema + Empty gRPC Server Skeleton

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the `.proto` schema, generate Rust client/server stubs via `tonic-build`, and replace the controller's Phase 1 immediate-return stub with a real `tonic` server that binds, listens, and returns `Status::unimplemented` for both RPCs. End state: `isengard controller` runs as a long-lived process that accepts gRPC connections, and `grpcurl` (or a tonic test client) sees the service.

**Architecture:** `isengard-proto` becomes the single source of truth for the wire schema. `isengard-controller`'s `run_controller` swaps from "init plugins, stop plugins, return" to "init plugins, bind tonic server, await ctrl_c, drain, stop plugins, return". The `Controller` service impl is a stub — both RPCs return `Status::unimplemented`. Phase 2c–2e wire the real handlers.

**Tech Stack:** Rust 2024 edition (1.85+), tonic 0.12, prost 0.13, tower 0.5, tokio (signal), tonic-reflection (so `grpcurl` can introspect — optional but very useful for development).

**Branch:** `next` (already pushed to GitHub, CI green). Do NOT push without explicit approval.

**Spec:** `docs/superpowers/specs/2026-04-30-phase-2-enrollment-sync-design.md` (sections §4 wire contract, §7 controller lifecycle, §13 sub-phase 2a).

---

## Scope

This plan covers **Phase 2a only**:
- Proto schema (with all variants scaffolded — Phase 3+ slots present but unused)
- `tonic-build` codegen wired into `isengard-proto`
- Controller binds the gRPC server, both RPCs return `Status::unimplemented`
- Reflection enabled (so `grpcurl` works during development)
- Integration test asserting the server binds, accepts a connection, and returns the right error code

It does **not** include:
- Token middleware (Phase 2c)
- Storage / `Inventory` (Phase 2b)
- Real Enroll handler (Phase 2c)
- Real Sync handler (Phase 2e)
- Agent client work (Phase 2d)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes (existing 18 tests + 1–2 new for Phase 2a)
3. `just ci-local` passes
4. `ISENGARD_TOKEN=test isengard controller --listen 127.0.0.1:9417` runs as a long-lived process; ctrl_c cleanly exits 0
5. `grpcurl -plaintext 127.0.0.1:9417 list` shows `isengard.v1.Controller` (reflection working)
6. `grpcurl -plaintext 127.0.0.1:9417 isengard.v1.Controller/Enroll` returns `Unimplemented`
7. Integration test exercises the same via tonic client
8. ~5–6 commits, each green on its own

---

## File Structure

```
isengard/
├── Cargo.toml                                           # MODIFY: + tonic, prost, tower, tonic-reflection workspace deps
├── crates/
│   ├── isengard-proto/
│   │   ├── Cargo.toml                                   # MODIFY: add deps + build-deps
│   │   ├── build.rs                                     # CREATE: tonic-build invoker
│   │   ├── proto/
│   │   │   └── isengard.v1.proto                        # CREATE: wire schema (full Phase 2 surface, Phase 3+ stubs included)
│   │   └── src/lib.rs                                   # MODIFY: include! the generated code; expose descriptor for reflection
│   ├── isengard-controller/
│   │   ├── Cargo.toml                                   # MODIFY: + isengard-proto, tonic, tonic-reflection, tokio signal
│   │   └── src/
│   │       ├── lib.rs                                   # MODIFY: ControllerOptions{listen}, run_controller binds tonic server
│   │       └── service.rs                               # CREATE: ControllerService impl returning Status::unimplemented
│   └── isengard/
│       ├── Cargo.toml                                   # MODIFY: token check at startup (ISENGARD_TOKEN env required)
│       └── src/main.rs                                  # MODIFY: read --listen + ISENGARD_TOKEN, build ControllerOptions
└── crates/isengard-controller/tests/
    └── server_skeleton.rs                               # CREATE: integration test (binds + Unimplemented)
```

---

## Task 1: Workspace deps for tonic stack

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add tonic + prost + tower + tonic-reflection to `[workspace.dependencies]`**

Append after the existing `# logging` block, before `# internal`:

```toml
# gRPC + protobuf
tonic = "0.12.3"
tonic-build = "0.12.3"
tonic-reflection = "0.12.3"
prost = "0.13.4"
prost-types = "0.13.4"
tower = "0.5.2"
```

- [ ] **Step 2: Verify the workspace still builds with no member touched yet**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -3
```

Expected: clean build (these are workspace-deps; no crate uses them yet, so they don't get pulled into Cargo.lock).

- [ ] **Step 3: Self-review**

- `Cargo.toml` lists the 6 new keys
- `cargo build --workspace` clean
- No member crate Cargo.toml touched yet

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml && git commit -m "chore(deps): add tonic + prost + tower + tonic-reflection to workspace"
```

---

## Task 2: Write `isengard.v1.proto`

**Files:**
- Create: `crates/isengard-proto/proto/isengard.v1.proto`

- [ ] **Step 1: Create the proto directory + file**

```bash
mkdir -p crates/isengard-proto/proto
cat > crates/isengard-proto/proto/isengard.v1.proto <<'EOF'
syntax = "proto3";

package isengard.v1;

// Controller-side service. Agents call into it.
//
// v1 wire surface, scoped for Phase 2 (Enroll + Sync only). Phase 3+ message
// variants are scaffolded as oneof fields so the schema stays stable.
service Controller {
  // One-shot at agent startup. Agent presents host metadata, receives a
  // controller-assigned ULID + the heartbeat interval the controller wants.
  rpc Enroll(EnrollRequest) returns (EnrollResponse);

  // Long-lived bidirectional stream. After enrolling, the agent opens this
  // stream once and reconnects on drop with exponential backoff.
  //
  //   client -> server: SyncHello (first frame), Heartbeat, then Phase 3+
  //   server -> client: HeartbeatAck, then Phase 3+ (Command, ConfigUpdate)
  rpc Sync(stream AgentMessage) returns (stream ControllerMessage);
}

// ============ ENROLL ============

message EnrollRequest {
  string fingerprint     = 1; // stable host id (hostname for v1; machine-id later)
  string hostname        = 2;
  string os              = 3; // e.g. "linux"
  string arch            = 4; // e.g. "x86_64"
  string agent_version   = 5;
  string docker_version  = 6;
}

message EnrollResponse {
  string agent_id                  = 1; // controller-assigned ULID
  uint32 heartbeat_interval_secs   = 2; // default 10
  uint64 server_time_ms            = 3;
}

// ============ SYNC ============

message AgentMessage {
  oneof payload {
    // First frame of every Sync stream. Identifies the agent post-enroll.
    SyncHello   hello     = 1;
    Heartbeat   heartbeat = 2;
    // Phase 3+: ContainerSnapshot snapshot = 3;
    // Phase 4+: Event              event    = 4;
  }
}

message SyncHello {
  string agent_id = 1;
}

message Heartbeat {
  uint64 ts_ms = 1; // agent's local clock at send time
}

message ControllerMessage {
  oneof payload {
    HeartbeatAck heartbeat_ack = 1;
    // Phase 3+: Command       command       = 2;
    // Phase 4+: ConfigUpdate  config_update = 3;
  }
}

message HeartbeatAck {
  uint64 server_time_ms = 1;
}
EOF
```

- [ ] **Step 2: Verify the proto file is well-formed**

```bash
test -f crates/isengard-proto/proto/isengard.v1.proto && wc -l crates/isengard-proto/proto/isengard.v1.proto
```

Expected: file exists, ~60 lines.

- [ ] **Step 3: Self-review**

- File exists at the right path
- All five message types present (`EnrollRequest`, `EnrollResponse`, `AgentMessage`, `ControllerMessage`, plus `SyncHello`, `Heartbeat`, `HeartbeatAck`)
- `service Controller` declares `Enroll` (unary) + `Sync` (bidi stream)
- Phase 3+ slots commented for later (`ContainerSnapshot`, `Event`, `Command`, `ConfigUpdate`)

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-proto/proto/ && git commit -m "feat(proto): isengard.v1.proto — Controller service, Enroll + Sync"
```

---

## Task 3: Wire `tonic-build` into `isengard-proto`

**Files:**
- Modify: `crates/isengard-proto/Cargo.toml`
- Create: `crates/isengard-proto/build.rs`
- Modify: `crates/isengard-proto/src/lib.rs`

- [ ] **Step 1: Update Cargo.toml with deps + build-deps**

```bash
cat > crates/isengard-proto/Cargo.toml <<'EOF'
[package]
name = "isengard-proto"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "gRPC service definitions and tonic-generated code"

[dependencies]
prost.workspace = true
prost-types.workspace = true
tonic.workspace = true

[build-dependencies]
tonic-build.workspace = true
EOF
```

- [ ] **Step 2: Write `build.rs`**

```bash
cat > crates/isengard-proto/build.rs <<'EOF'
//! Compiles `proto/isengard.v1.proto` to Rust at build time, plus emits a
//! file descriptor set for runtime reflection.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .file_descriptor_set_path(out_dir.join("isengard_descriptor.bin"))
        .compile_protos(&["proto/isengard.v1.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/isengard.v1.proto");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
EOF
```

- [ ] **Step 3: Wire `lib.rs` to include the generated code + expose the descriptor**

```bash
cat > crates/isengard-proto/src/lib.rs <<'EOF'
//! gRPC service definitions for Isengard v1.
//!
//! Generated by `tonic-build` from `proto/isengard.v1.proto`. The generated
//! module is re-exported as `pb` and used by `isengard-controller` (server)
//! and `isengard-agent` (client).

#![allow(clippy::all, missing_docs)]

pub mod pb {
    tonic::include_proto!("isengard.v1");
}

/// Encoded `FileDescriptorSet` for the v1 service. Used by `tonic-reflection`
/// to expose the schema over the wire (so `grpcurl` and similar tools work
/// without a local copy of the .proto file).
pub const FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("isengard_descriptor");
EOF
```

- [ ] **Step 4: Verify the codegen works**

```bash
cd ~/Projects/isengard && cargo build -p isengard-proto 2>&1 | tail -5
cd ~/Projects/isengard && cargo doc -p isengard-proto --no-deps 2>&1 | tail -3
```

Expected: `isengard-proto` builds clean. `cargo doc` shows the `pb` module with `Controller`, `EnrollRequest`, `EnrollResponse`, etc.

- [ ] **Step 5: Run clippy on the generated code**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-proto --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean (note the `#![allow(clippy::all)]` in `lib.rs` suppresses lints in the generated code, which is conventional).

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-proto/ Cargo.lock && git commit -m "feat(proto): tonic-build codegen + file descriptor set for reflection"
```

---

## Task 4: Stub `ControllerService` impl

**Files:**
- Modify: `crates/isengard-controller/Cargo.toml`
- Create: `crates/isengard-controller/src/service.rs`
- Modify: `crates/isengard-controller/src/lib.rs`

The service impl returns `Status::unimplemented` for both RPCs. Phase 2c–2e replace these with real handlers. The point of this task is to validate the wire end-to-end.

- [ ] **Step 1: Add deps to `isengard-controller/Cargo.toml`**

```bash
cat > crates/isengard-controller/Cargo.toml <<'EOF'
[package]
name = "isengard-controller"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Controller-mode runtime"

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
isengard-core.workspace = true
isengard-proto.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tonic.workspace = true
tonic-reflection.workspace = true
tower.workspace = true
tracing.workspace = true
EOF
```

(Workspace deps needs `isengard-proto`. If it isn't already in `[workspace.dependencies]`, add it next to the other internal entries.)

- [ ] **Step 2: Verify `isengard-proto` is in workspace.dependencies**

```bash
cd ~/Projects/isengard && grep -E '^isengard-proto' Cargo.toml
```

Expected: `isengard-proto = { path = "crates/isengard-proto", version = "0.1.0-alpha" }`.

If the line is missing, add it under the `# internal` block.

- [ ] **Step 3: Write the stub service**

```bash
cat > crates/isengard-controller/src/service.rs <<'EOF'
//! Stub `Controller` gRPC service. Both RPCs return [`Status::unimplemented`].
//! Phase 2c–2e replace these with real handlers.

use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{
    AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Default, Clone)]
pub struct ControllerService;

#[tonic::async_trait]
impl Controller for ControllerService {
    type SyncStream = ReceiverStream<Result<ControllerMessage, Status>>;

    async fn enroll(
        &self,
        _request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        tracing::debug!("Enroll RPC reached stub — returning Unimplemented");
        Err(Status::unimplemented("Enroll handler lands in Phase 2c"))
    }

    async fn sync(
        &self,
        _request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        tracing::debug!("Sync RPC reached stub — returning Unimplemented");
        Err(Status::unimplemented("Sync handler lands in Phase 2e"))
    }
}
EOF
```

- [ ] **Step 4: Add `tokio-stream` to workspace + this crate's deps**

```bash
# Append tokio-stream to workspace.dependencies in root Cargo.toml.
# Use a sed-replace under the "# gRPC + protobuf" block; if that doesn't match,
# just open Cargo.toml and add the line manually.
```

Edit `~/Projects/isengard/Cargo.toml` so the `# gRPC + protobuf` block reads:

```toml
# gRPC + protobuf
tonic = "0.12.3"
tonic-build = "0.12.3"
tonic-reflection = "0.12.3"
prost = "0.13.4"
prost-types = "0.13.4"
tower = "0.5.2"
tokio-stream = "0.1.17"
```

Then add `tokio-stream.workspace = true` to `crates/isengard-controller/Cargo.toml`'s `[dependencies]`.

- [ ] **Step 5: Verify the service compiles**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
```

Expected: clean build. `service.rs` compiles, the trait impl is correct.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/ Cargo.toml Cargo.lock && git commit -m "feat(controller): stub Controller service returning Status::unimplemented"
```

---

## Task 5: `run_controller` binds tonic server, awaits ctrl_c

**Files:**
- Modify: `crates/isengard-controller/src/lib.rs`

Phase 1's `run_controller` lifecycles plugins and returns immediately. Phase 2a's `run_controller` lifecycles plugins, then binds the tonic server and blocks on `ctrl_c`.

- [ ] **Step 1: Rewrite `run_controller`**

```bash
cat > crates/isengard-controller/src/lib.rs <<'EOF'
//! Controller-mode runtime: load plugins, bind the gRPC server, await
//! shutdown, drain, stop plugins, exit.

mod service;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use isengard_core::{HostMode, Plugin, PluginContext, registrations_for};
use isengard_proto::FILE_DESCRIPTOR_SET;
use isengard_proto::pb::controller_server::ControllerServer;
use tokio::signal;
use tonic::transport::Server;
use tracing::{info, instrument};

pub use service::ControllerService;

/// Options for running the controller.
#[derive(Debug, Clone)]
pub struct ControllerOptions {
    /// Address the gRPC server binds to (e.g. `0.0.0.0:9417`).
    pub listen: SocketAddr,
    /// Optional config tree (per-plugin slices keyed by plugin name).
    pub config: serde_json::Value,
}

/// Discover and instantiate every plugin that advertises `Capability::Controller`.
pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Controller)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

/// Run controller-mode: init+start every plugin, bind the gRPC server, await
/// ctrl_c, drain in-flight requests, stop every plugin, return.
#[instrument(skip(opts), fields(listen = %opts.listen))]
pub async fn run_controller(opts: ControllerOptions) -> Result<()> {
    info!("starting controller");

    // -- plugin init/start ----------------------------------------------------
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
        let ctx = PluginContext::new(HostMode::Controller, plugin_config);
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // -- gRPC server ----------------------------------------------------------
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()
        .context("building reflection service")?;

    let svc = ControllerServer::new(ControllerService);

    info!("gRPC server listening");
    let serve_fut = Server::builder()
        .add_service(svc)
        .add_service(reflection)
        .serve_with_shutdown(opts.listen, async {
            // SIGINT / Ctrl-C terminates serve_with_shutdown gracefully.
            // serve waits for in-flight requests to drain before returning.
            let _ = signal::ctrl_c().await;
            info!("shutdown signal received, draining");
        });

    serve_fut.await.context("gRPC server error")?;

    // -- plugin stop ----------------------------------------------------------
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!("controller exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration tests against a running server live in
    // `tests/server_skeleton.rs`. The Phase-1-style "default options" smoke
    // test no longer applies because the runner now blocks on ctrl_c.
    #[test]
    fn dummy() {
        // placeholder so the unit-test target stays alive for nextest
        assert_eq!(2 + 2, 4);
    }
}
EOF
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 3: Verify clippy passes**

```bash
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/lib.rs && git commit -m "feat(controller): bind tonic server with reflection, await ctrl_c"
```

---

## Task 6: Plumb `--listen` and `ISENGARD_TOKEN` requirement through the binary

**Files:**
- Modify: `crates/isengard/src/main.rs`

The CLI already defines `--listen`. Now we need to actually pass it into `ControllerOptions` and require `ISENGARD_TOKEN` to be set at startup (the token is unused by Phase 2a's stub but the requirement lands here so 2c just plugs in the middleware).

- [ ] **Step 1: Update `main.rs`**

Open `crates/isengard/src/main.rs`. Replace the `Command::Controller { listen }` arm with:

```rust
        Command::Controller { listen } => {
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            // Token is unused in Phase 2a (the middleware lands in Phase 2c)
            // but checked at startup so misconfiguration fails loud and early.
            let _ = token;

            let listen_addr: std::net::SocketAddr = listen.parse()
                .map_err(|e| anyhow::anyhow!("invalid --listen address {listen:?}: {e}"))?;

            tracing::info!(%listen_addr, "controller mode");
            isengard_controller::run_controller(isengard_controller::ControllerOptions {
                listen: listen_addr,
                config: serde_json::Value::Object(Default::default()),
            }).await
        }
```

Use the Edit tool to apply that change in place.

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Projects/isengard && cargo build -p isengard 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 3: Smoke run — without token, must fail with a clear message**

```bash
cd ~/Projects/isengard && ./target/debug/isengard controller 2>&1 | tail -3
```

Expected: process exits non-zero with the message `ISENGARD_TOKEN env var must be set`.

- [ ] **Step 4: Smoke run — with token, must bind and listen**

```bash
cd ~/Projects/isengard && timeout 3 env ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9418 2>&1 | tail -10
echo "(timed out at 3s = process was alive — that's the success signal)"
```

Expected: log lines for plugin lifecycle + `gRPC server listening` + `listen=127.0.0.1:9418`. The process is killed by timeout after 3s.

- [ ] **Step 5: Smoke run — verify reflection works (requires `grpcurl`)**

If `grpcurl` is installed:

```bash
# Run server in the background:
cd ~/Projects/isengard && env ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9419 &
SERVER_PID=$!
sleep 1
# Reflection must list the service:
grpcurl -plaintext 127.0.0.1:9419 list
# An RPC call must return Unimplemented:
grpcurl -plaintext -d '{}' 127.0.0.1:9419 isengard.v1.Controller/Enroll || true
kill $SERVER_PID
```

Expected:
- `list` prints `grpc.reflection.v1.ServerReflection` and `isengard.v1.Controller`
- `Enroll` returns `ERROR: Code: Unimplemented`

If `grpcurl` is not installed: skip Step 5 and rely on the integration test in Task 7.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard/src/main.rs && git commit -m "feat(bin): controller --listen plumb-through + require ISENGARD_TOKEN"
```

---

## Task 7: Integration test — server binds, returns Unimplemented

**Files:**
- Create: `crates/isengard-controller/tests/server_skeleton.rs`

This test programmatically spawns the controller library, opens a tonic client, and asserts the stub returns the right error code. It's the canonical proof that the wire works end-to-end without depending on `grpcurl`.

- [ ] **Step 1: Add tonic + dev tokio to controller dev-deps**

Modify `crates/isengard-controller/Cargo.toml` to add a `[dev-dependencies]` block:

```toml
[dev-dependencies]
isengard-proto = { workspace = true }
tonic = { workspace = true }
tokio = { workspace = true }
```

(Yes — `isengard-proto` and `tonic` are already in `[dependencies]`. Adding them to `[dev-dependencies]` makes the tests link them as dev-only. If Cargo complains about the duplication, omit the dev block — they're already accessible.)

If the duplication causes warnings, drop the `[dev-dependencies]` block. The deps from `[dependencies]` are visible to integration tests.

- [ ] **Step 2: Write the test**

```bash
mkdir -p crates/isengard-controller/tests
cat > crates/isengard-controller/tests/server_skeleton.rs <<'EOF'
//! Phase 2a integration test: spin up the controller library on an ephemeral
//! port, connect a tonic client, assert both RPCs return Unimplemented.

use std::net::SocketAddr;
use std::time::Duration;

use isengard_controller::{ControllerOptions, run_controller};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::EnrollRequest;
use tonic::Code;

async fn spawn_controller_on_ephemeral_port() -> SocketAddr {
    // Pick an ephemeral port by binding 0 first.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // release the port so tonic can rebind

    tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            config: serde_json::Value::Object(Default::default()),
        }).await;
    });

    // Wait for the server to bind; poll up to 2s.
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

#[tokio::test]
async fn enroll_returns_unimplemented() {
    let addr = spawn_controller_on_ephemeral_port().await;
    let mut client = ControllerClient::connect(format!("http://{addr}"))
        .await
        .expect("connect");

    let err = client
        .enroll(EnrollRequest::default())
        .await
        .expect_err("Enroll should return an error in Phase 2a");

    assert_eq!(err.code(), Code::Unimplemented, "got: {err:?}");
}

#[tokio::test]
async fn server_accepts_concurrent_connections() {
    let addr = spawn_controller_on_ephemeral_port().await;

    // Open three clients in parallel; all should connect and receive
    // Unimplemented from Enroll.
    let mut handles = Vec::new();
    for _ in 0..3 {
        handles.push(tokio::spawn(async move {
            let mut client = ControllerClient::connect(format!("http://{addr}"))
                .await
                .expect("connect");
            let err = client.enroll(EnrollRequest::default()).await.unwrap_err();
            assert_eq!(err.code(), Code::Unimplemented);
        }));
    }
    for h in handles {
        h.await.expect("task");
    }
}
EOF
```

- [ ] **Step 3: Run the integration tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller --test server_skeleton 2>&1 | tail -10
```

Expected: 2 tests pass. If `run_controller` blocks on `ctrl_c` and never returns, the spawned task stays running for the test process lifetime — that's fine. The clients still connect and the assertions pass.

If the test hangs: check that `serve_with_shutdown` is the one being used (not `serve`), and that the spawned task is `tokio::spawn` not `tokio::task::spawn_blocking`.

- [ ] **Step 4: Run the full test suite**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "running|passed|failed" | tail -10
```

Expected: previously-green 18 tests + 2 new = 20 tests. All pass.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/Cargo.toml crates/isengard-controller/tests/ Cargo.lock && git commit -m "test(controller): integration test — server binds + RPCs return Unimplemented"
```

---

## Task 8: Run full local CI gate

- [ ] **Step 1: Run the gate**

```bash
cd ~/Projects/isengard && just ci-local
```

Expected: fmt-check + clippy + tests all pass. Final line `✓ ci-local passed`.

- [ ] **Step 2: Smoke-run controller end-to-end one more time**

```bash
cd ~/Projects/isengard && timeout 2 env ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9420 2>&1 | tail -5
echo "(2s timeout = the long-lived server worked)"
```

Expected: `gRPC server listening` and `listen=127.0.0.1:9420` in the output.

- [ ] **Step 3: Tag the sub-phase**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2a -m "phase 2a: proto + tonic-build + empty server skeleton"
git tag -l | grep phase2a
```

Tag stays local until pushed.

---

## Self-review

After writing this plan, look at the spec sections referenced (§4 wire contract, §7 controller lifecycle, §13 sub-phase 2a) and check:

| Requirement | Plan task |
|---|---|
| Proto schema with all v1 + scaffolded v2+ messages | Task 2 |
| `tonic-build` codegen | Task 3 |
| Reflection (so `grpcurl` works) | Task 3 + 5 |
| Controller binds on `--listen` | Task 5 + 6 |
| `ISENGARD_TOKEN` required at startup | Task 6 |
| Both RPCs return `Status::unimplemented` | Task 4 |
| `tokio::signal::ctrl_c` graceful shutdown | Task 5 |
| Integration test asserts the wire works | Task 7 |
| Plugin lifecycle still runs (init/start/stop) | Task 5 (kept from Phase 1) |
| `dev` plugin still loads in controller mode | Task 5 (kept) |
| Workspace builds clean, all tests pass | Task 8 |

No placeholders. Every code step contains complete code. Every command step has expected output.

**Spec deviations (intentional):**
- The plan adds `tonic-reflection` to support `grpcurl` during development. The spec mentions this is helpful but doesn't mandate it. Including it costs ~50KB binary size and is worth it for ops introspection.
- The plan keeps the Phase 1 plugin lifecycle bookends (init/start/stop) around the gRPC server. This is forward-compatible — Phase 4 plugins (notifier) need to be running during `serve_with_shutdown`.

---

## Execution Handoff

Plan complete. Two execution options:

**1. Subagent-Driven (recommended)** — fresh subagent per task with full plan context, review between tasks, mark complete in TodoWrite. Same workflow as Phase 0+1.

**2. Inline Execution** — execute tasks in this session using the executing-plans skill, batch with checkpoints.

The plan is small enough (8 tasks, ~30 steps) that either works. Subagent-driven keeps the controller's context clean if Phase 2b–2g follow immediately.
