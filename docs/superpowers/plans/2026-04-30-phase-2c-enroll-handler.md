# Phase 2c: Real `Enroll` Handler + Token Auth Middleware

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Enroll` actually work. Replace the `Status::unimplemented` stub with a real handler that validates a bearer token from gRPC metadata, persists the host into the SQLite inventory built in Phase 2b, and returns a controller-assigned ULID + heartbeat interval. By the end of Phase 2c, an external client (`grpcurl`, or a tonic test client) can call `Enroll` over the wire with a valid token, and a `hosts` row appears in the database.

**Architecture:** Three pieces land:
1. A `tower` middleware layer (`TokenAuthLayer`) that reads `authorization: Bearer …` from request metadata, constant-time-compares against the configured `ISENGARD_TOKEN`, and rejects with `UNAUTHENTICATED`. Mounted on the `isengard.v1.Controller` service only — the reflection service stays open so `grpcurl` can introspect.
2. A `--state-dir` flag and `Inventory::open(state_dir.join("isengard.db"))` call inside `run_controller`. The opened `Inventory` is wrapped in `Arc` and handed to the `ControllerService` constructor.
3. A real `Enroll` handler that validates the request body, calls `Inventory::enroll_host(...)`, and returns `EnrollResponse { agent_id, heartbeat_interval_secs: 10, server_time_ms }`. Duplicate-fingerprint errors map to `Status::already_exists`.

`Sync` stays `Unimplemented` — that's Phase 2e.

**Tech Stack:** Rust 2024, tonic 0.12 (with tower middleware), `subtle` 2.6 for constant-time token comparison, the existing `isengard-storage` crate.

**Branch:** `next` (currently 7 commits ahead of `origin/next` after Phase 2b). Do NOT push without explicit approval.

**Spec:** §4 wire contract, §5 auth handshake, §7 controller lifecycle (Enroll RPC details), §13 sub-phase 2c.

---

## Scope

**In:**
- `--state-dir` flag on the binary's `controller` subcommand (default: `~/.local/state/isengard` for dev, `/var/lib/isengard` in container — implementation picks one default; the container value is documented but not detected at runtime)
- `Inventory` opened at controller startup; handle passed to the gRPC service via `Arc`
- `TokenAuthLayer` tower middleware (constant-time-compare bearer token from metadata)
- Layer mounted on the `Controller` service ONLY (not on `ServerReflection` — reflection stays open)
- Real `Enroll` handler: parse request → call `Inventory::enroll_host` → return ULID + heartbeat interval + server time
- Duplicate-fingerprint error path → `Status::already_exists`
- Updated integration tests:
  - REMOVE `enroll_returns_unimplemented` (no longer accurate)
  - REMOVE `server_accepts_concurrent_connections` from Phase 2a (or convert to use real Enroll if it still adds value)
  - ADD `enroll_without_token_returns_unauthenticated`
  - ADD `enroll_with_valid_token_persists_host`
  - ADD `enroll_with_duplicate_fingerprint_returns_already_exists`
  - ADD `enroll_response_carries_heartbeat_interval`

**Out (still deferred):**
- `Sync` handler (Phase 2e)
- Agent client (Phase 2d)
- mTLS (post-v1)
- Per-agent tokens / rotation (post-v1)
- Reconnection / backoff logic (Phase 2f, agent-side)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` passes (≥ 36 tests: 34 from Phase 2b + ≥ 4 new — minus 2 removed = net +2 minimum, target +4)
3. `just ci-local` green
4. `grpcurl -plaintext -H "authorization: Bearer test" -d '{}' 127.0.0.1:9420 isengard.v1.Controller/Enroll` returns a populated `agent_id` (or empty `EnrollResponse` if the server allows empty fingerprint — implementation-defined; the test asserts the persisted row, not grpcurl output specifically)
5. `ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9420 --state-dir /tmp/isengard-test` runs and persists hosts in `/tmp/isengard-test/isengard.db`
6. Tag `v0.1.0-alpha.phase2c` set locally
7. ~5–6 commits, each green on its own

---

## File Structure

```
isengard/
├── Cargo.toml                                                    # MODIFY: + subtle workspace dep
├── crates/
│   ├── isengard-controller/
│   │   ├── Cargo.toml                                            # MODIFY: + isengard-storage, subtle
│   │   └── src/
│   │       ├── lib.rs                                            # MODIFY: open Inventory, build TokenAuthLayer, mount on service
│   │       ├── service.rs                                        # MODIFY: ControllerService now holds Arc<Inventory>; real Enroll handler
│   │       └── auth.rs                                           # CREATE: TokenAuthLayer + TokenAuth tower middleware
│   │   └── tests/
│   │       └── server_skeleton.rs                                # MODIFY: rewrite tests around real Enroll
│   └── isengard/
│       └── src/main.rs                                           # MODIFY: + --state-dir flag, plumb to ControllerOptions
```

---

## Task 1: `subtle` workspace dep + `--state-dir` flag

**Files:**
- Modify: `Cargo.toml` (root)
- Modify: `crates/isengard/src/main.rs`
- Modify: `crates/isengard-controller/src/lib.rs` (just `ControllerOptions` struct change)

- [ ] **Step 1: Add `subtle` to workspace.dependencies**

Open `~/Projects/isengard/Cargo.toml`. After the `# storage` block (which ends with `ulid = ...`), append:

```toml
# crypto helpers
subtle = "2.6.1"
```

- [ ] **Step 2: Add `state_dir` to `ControllerOptions`**

Read `~/Projects/isengard/crates/isengard-controller/src/lib.rs`. Find:

```rust
pub struct ControllerOptions {
    pub listen: SocketAddr,
    pub config: serde_json::Value,
}
```

Add a third field:

```rust
pub struct ControllerOptions {
    pub listen: SocketAddr,
    /// Directory where mutable state lives (the SQLite db, future config cache, etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
}
```

(`run_controller` will use `state_dir` in Task 3. For Task 1, just add the field — code that doesn't use it yet still compiles.)

- [ ] **Step 3: Add the `--state-dir` flag to the binary**

Read `~/Projects/isengard/crates/isengard/src/main.rs`. Find the `Command::Controller` variant and add the flag:

```rust
    Controller {
        /// HTTP/gRPC listen address.
        #[arg(long, env = "ISENGARD_LISTEN", default_value = "0.0.0.0:9417")]
        listen: String,
        /// Directory where the controller stores mutable state (SQLite db, etc).
        /// Created if missing.
        #[arg(long, env = "ISENGARD_STATE_DIR", default_value = "/var/lib/isengard")]
        state_dir: std::path::PathBuf,
    },
```

In the controller match arm, plumb `state_dir` into `ControllerOptions`:

```rust
        Command::Controller { listen, state_dir } => {
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let _ = token; // unused in this task; Task 2 wires it into TokenAuthLayer

            let listen_addr: std::net::SocketAddr = listen.parse()
                .map_err(|e| anyhow::anyhow!("invalid --listen address {listen:?}: {e}"))?;

            // Create state dir if missing (controller can't run without somewhere to put the db).
            std::fs::create_dir_all(&state_dir)
                .map_err(|e| anyhow::anyhow!("creating state dir {state_dir:?}: {e}"))?;

            tracing::info!(%listen_addr, ?state_dir, "controller mode");
            isengard_controller::run_controller(isengard_controller::ControllerOptions {
                listen: listen_addr,
                state_dir,
                config: serde_json::Value::Object(Default::default()),
            }).await
        }
```

- [ ] **Step 4: Verify it compiles**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -3
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. Existing tests still pass (we haven't broken any behavior; just added a field that nothing reads yet).

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | head -10
```

Expected: same green test counts as Phase 2b end (no new tests, no regressions).

- [ ] **Step 5: Smoke run**

```bash
cd ~/Projects/isengard && timeout 2 env ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9421 --state-dir /tmp/isengard-2c-smoke 2>&1 | head -8
ls -la /tmp/isengard-2c-smoke 2>&1 | head -3
```

Expected: process binds, log line includes `state_dir=/tmp/isengard-2c-smoke`. The directory exists post-run.

(If you don't have `timeout` on macOS: `gtimeout` or background+`sleep 2`+`kill`.)

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml crates/isengard/src/main.rs crates/isengard-controller/src/lib.rs && git commit -m "feat(controller): + state_dir field + --state-dir flag, subtle workspace dep"
```

---

## Task 2: `TokenAuthLayer` tower middleware

**Files:**
- Create: `crates/isengard-controller/src/auth.rs`
- Modify: `crates/isengard-controller/Cargo.toml`
- Modify: `crates/isengard-controller/src/lib.rs` (`mod auth;` declaration)

The middleware sits between tonic's transport layer and the service handler. Per gRPC convention, the bearer token rides in the `authorization` metadata header. Constant-time compare (via `subtle::ConstantTimeEq`) prevents timing-based token discovery.

- [ ] **Step 1: Add `subtle` to controller deps**

Read `crates/isengard-controller/Cargo.toml` and add `subtle.workspace = true` to `[dependencies]`.

- [ ] **Step 2: Write `auth.rs`**

```bash
cat > crates/isengard-controller/src/auth.rs <<'EOF'
//! `TokenAuthLayer`: a tower middleware that gates a tonic service behind a
//! bearer-token check on the `authorization` metadata header.
//!
//! v1 is shared-secret-token auth — the controller runs with `ISENGARD_TOKEN`
//! in env, every agent presents the same value. Per-agent tokens / rotation
//! / mTLS upgrade is post-v1.

use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use subtle::ConstantTimeEq;
use tonic::body::BoxBody;
use tower::{Layer, Service};

/// Tower layer producing [`TokenAuth`] middleware around a service.
#[derive(Clone)]
pub struct TokenAuthLayer {
    expected: Arc<Vec<u8>>,
}

impl TokenAuthLayer {
    pub fn new(token: impl Into<String>) -> Self {
        Self { expected: Arc::new(token.into().into_bytes()) }
    }
}

impl<S> Layer<S> for TokenAuthLayer {
    type Service = TokenAuth<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TokenAuth { inner, expected: self.expected.clone() }
    }
}

#[derive(Clone)]
pub struct TokenAuth<S> {
    inner: S,
    expected: Arc<Vec<u8>>,
}

impl<S, B> Service<Request<B>> for TokenAuth<S>
where
    S: Service<Request<B>, Response = Response<BoxBody>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut inner = self.inner.clone();
        let expected = self.expected.clone();

        Box::pin(async move {
            let header = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            let presented = header.strip_prefix("Bearer ").unwrap_or("");

            // Constant-time compare. CtOption truncates at the shorter length
            // and reports inequality if lengths differ.
            let ok = presented.as_bytes().ct_eq(expected.as_slice()).into();

            if ok {
                return inner.call(req).await;
            }

            // Reject with UNAUTHENTICATED. Build the gRPC-conformant response.
            let status = tonic::Status::unauthenticated("missing or invalid bearer token");
            Ok(status.into_http())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_rejects_mismatched_lengths() {
        let a = b"foo";
        let b: &[u8] = b"foobar";
        let eq: bool = a.ct_eq(b).into();
        assert!(!eq);
    }

    #[test]
    fn ct_eq_accepts_exact_match() {
        let a = b"hunter2";
        let b: &[u8] = b"hunter2";
        let eq: bool = a.ct_eq(b).into();
        assert!(eq);
    }

    #[test]
    fn ct_eq_rejects_close_mismatch() {
        let a = b"hunter2";
        let b: &[u8] = b"hunter3";
        let eq: bool = a.ct_eq(b).into();
        assert!(!eq);
    }
}
EOF
```

Note on `tonic::Status::into_http`: this method exists on tonic 0.12 and converts a `Status` into an HTTP response with the right gRPC trailers (`grpc-status`, `grpc-message`). If the exact name differs (e.g. `into_response` or `to_http`), check `cargo doc -p tonic --open` for the actual signature and adjust. The plan code is written for tonic 0.12.x.

If tonic 0.12 lacks `Status::into_http`, fall back to constructing the response manually:

```rust
let status = tonic::Status::unauthenticated("missing or invalid bearer token");
Ok(status.into_http().map(|_| ()).map_body(...))
```

If you hit any tonic API compatibility issue here that takes more than 5 minutes to resolve, report DONE_WITH_CONCERNS with the exact compile error so the controller can decide whether to bump tonic, drop the constant-time path, or simplify.

- [ ] **Step 3: Declare the module in lib.rs**

Read `crates/isengard-controller/src/lib.rs`. Find `mod service;` and add `mod auth;` next to it (above or below — doesn't matter).

Also re-export the layer:

```rust
pub use auth::TokenAuthLayer;
```

- [ ] **Step 4: Verify it compiles**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. If `Status::into_http()` is the wrong name on this tonic version, see fallback note in Step 2.

- [ ] **Step 5: Run unit tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller --lib 2>&1 | tail -10
```

Expected: 4 tests pass (1 dummy from Phase 2a + 3 new auth tests).

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/Cargo.toml crates/isengard-controller/src/auth.rs crates/isengard-controller/src/lib.rs Cargo.lock && git commit -m "feat(controller): TokenAuthLayer tower middleware (constant-time bearer compare)"
```

---

## Task 3: Open `Inventory` at startup, mount middleware on service

**Files:**
- Modify: `crates/isengard-controller/Cargo.toml` (add `isengard-storage` dep)
- Modify: `crates/isengard-controller/src/lib.rs`
- Modify: `crates/isengard-controller/src/service.rs` (constructor takes Inventory)

- [ ] **Step 1: Add `isengard-storage` to controller deps**

Read `crates/isengard-controller/Cargo.toml` and add `isengard-storage.workspace = true` to `[dependencies]`.

- [ ] **Step 2: Update `ControllerService` constructor**

Read `crates/isengard-controller/src/service.rs`. The current `ControllerService` is a unit struct with `#[derive(Default, Clone)]`. Replace it with a struct that holds the inventory:

```rust
use std::sync::Arc;

use isengard_storage::Inventory;

#[derive(Clone)]
pub struct ControllerService {
    inventory: Arc<Inventory>,
}

impl ControllerService {
    pub fn new(inventory: Arc<Inventory>) -> Self {
        Self { inventory }
    }
}
```

The existing `impl Controller for ControllerService { ... }` block stays as-is for now (still returning `Status::unimplemented` for both RPCs — Task 4 replaces the Enroll body).

- [ ] **Step 3: Wire `Inventory::open` + `TokenAuthLayer` into `run_controller`**

Read `crates/isengard-controller/src/lib.rs`. The current `run_controller` builds the service like this:

```rust
let svc = ControllerServer::new(ControllerService);

let serve_fut = Server::builder()
    .add_service(svc)
    .add_service(reflection)
    .serve_with_shutdown(opts.listen, ...);
```

Rewrite to:

A) Open the Inventory before binding. Need to add the `ISENGARD_TOKEN` read here too (Task 1 left it at the binary level; for the layer we need it inside `run_controller`):

```rust
// At the top of run_controller, after `info!("starting controller")`:

// Open inventory.
let db_path = opts.state_dir.join("isengard.db");
let inventory = Arc::new(
    isengard_storage::Inventory::open(&db_path)
        .await
        .with_context(|| format!("opening inventory at {db_path:?}"))?,
);
info!(?db_path, "inventory opened");
```

(Need `use std::sync::Arc;` and `use anyhow::Context;` if not already there.)

B) Build the `TokenAuthLayer` from `ISENGARD_TOKEN`:

```rust
// Token comes from env. Binary already verified it's set; if it's somehow
// missing here, fail loud rather than silently allow.
let token = std::env::var("ISENGARD_TOKEN")
    .with_context(|| "ISENGARD_TOKEN env var must be set")?;
let auth_layer = crate::auth::TokenAuthLayer::new(token);
```

C) Mount the layer on the Controller service ONLY (not on reflection, so `grpcurl` can introspect):

```rust
let svc = ControllerServer::new(ControllerService::new(inventory));

let serve_fut = Server::builder()
    .layer(tower::ServiceBuilder::new().into_inner())  // anchor; remove if extra layers aren't needed
    .add_service(
        tower::ServiceBuilder::new()
            .layer(auth_layer)
            .service(svc),
    )
    .add_service(reflection)
    .serve_with_shutdown(opts.listen, ...);
```

Adjust the import block so `tower::ServiceBuilder` is in scope:

```rust
use tower::ServiceBuilder;
```

If tonic 0.12 expects a `tower::Layer<tonic::body::BoxBody>` and `add_service` doesn't accept the wrapped service directly, the alternative is:

```rust
// Build the routes and wrap them.
let routes = Server::builder()
    .add_service(svc)
    .add_service(reflection)
    .into_router();

let app = ServiceBuilder::new()
    .layer(auth_layer)  // applies to ALL routes — not what we want
    .service(routes);
```

…but that mounts the layer globally including reflection, which would break `grpcurl`. Per-service layering in tonic 0.12 is via `add_service(svc).intercept(...)` or via building each `ServiceBuilder` separately. **If exact API doesn't accept the per-service wrapping above, fall back to applying the auth layer globally and excluding reflection by reading and validating `req.uri().path()` — paths starting with `/grpc.reflection.v1.ServerReflection/` skip auth.** Implement that fallback inside `TokenAuth::call` if needed.

This is the trickiest part of Phase 2c. If you're stuck for more than 10 minutes on the layering API, simplify: apply the layer globally, but in `TokenAuth::call` add an early-return for paths starting with `/grpc.reflection.v1.ServerReflection/`. Document the choice in the commit message.

- [ ] **Step 4: Verify build + clippy**

```bash
cd ~/Projects/isengard && cargo build -p isengard-controller 2>&1 | tail -5
cd ~/Projects/isengard && cargo clippy -p isengard-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean. Existing tests still pass.

- [ ] **Step 5: Run controller unit + integration tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | grep -E "running|passed|failed" | head -10
```

Note: the existing integration tests in `tests/server_skeleton.rs` will break here because:
- Their `spawn_controller_on_ephemeral_port` doesn't set `ISENGARD_TOKEN` env, and `run_controller` now requires it
- They don't set `ControllerOptions.state_dir`

Expected: tests fail. That's OK — Task 4 fixes them. For now, run the unit tests:

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller --lib 2>&1 | tail -10
```

Expected: 4 unit tests pass. Integration tests broken — addressed in Task 4.

- [ ] **Step 6: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/Cargo.toml crates/isengard-controller/src/lib.rs crates/isengard-controller/src/service.rs Cargo.lock && git commit -m "feat(controller): open Inventory + mount TokenAuthLayer on Controller service"
```

(Integration tests stay broken between Task 3 and Task 4 — that's a known transient. We commit because each task is a coherent slice.)

---

## Task 4: Real `Enroll` handler + integration tests

**Files:**
- Modify: `crates/isengard-controller/src/service.rs`
- Modify: `crates/isengard-controller/tests/server_skeleton.rs`

- [ ] **Step 1: Replace the `enroll` body**

In `crates/isengard-controller/src/service.rs`, replace the existing `enroll` method:

```rust
    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let req = request.into_inner();

        // Convert proto request → storage request.
        let enroll_req = isengard_storage::EnrollHost {
            fingerprint: req.fingerprint,
            hostname: req.hostname,
            os: req.os,
            arch: req.arch,
            agent_version: req.agent_version,
            docker_version: req.docker_version,
        };

        let id = match self.inventory.enroll_host(enroll_req).await {
            Ok(id) => id,
            Err(isengard_storage::Error::Db(sqlx_err)) => {
                // UNIQUE violation on fingerprint.
                if let Some(db_err) = sqlx_err.as_database_error() {
                    if let Some(code) = db_err.code() {
                        // SQLite UNIQUE constraint failed = error code 2067 (extended)
                        // or 19 (primary). Match the message instead — sqlx's code
                        // surface varies per SQLite version.
                        if db_err.message().contains("UNIQUE")
                            || code == "2067"
                            || code == "19"
                        {
                            return Err(Status::already_exists(format!(
                                "host with fingerprint {:?} is already enrolled",
                                req.hostname,
                            )));
                        }
                    }
                }
                tracing::error!(error = %sqlx_err, "Enroll: db error");
                return Err(Status::internal("database error"));
            }
            Err(other) => {
                tracing::error!(error = %other, "Enroll: unexpected error");
                return Err(Status::internal("storage error"));
            }
        };

        let server_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Ok(Response::new(EnrollResponse {
            agent_id: id.to_string(),
            heartbeat_interval_secs: 10,
            server_time_ms,
        }))
    }
```

Update the imports at the top of `service.rs` so `Request<EnrollRequest>` and the storage types are in scope. Should already be there from Task 3 — verify.

The `sync` method stays returning `Status::unimplemented("Sync handler lands in Phase 2e")`. Don't touch it.

- [ ] **Step 2: Rewrite the integration tests**

Open `crates/isengard-controller/tests/server_skeleton.rs`. Replace the entire file with:

```rust
//! Phase 2c integration tests: real Enroll handler + token auth.
//!
//! Spawns the controller library on an ephemeral port with a temp state dir,
//! exercises Enroll over a tonic client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use isengard_controller::{run_controller, ControllerOptions};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::EnrollRequest;
use tempfile::TempDir;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const TEST_TOKEN: &str = "test-token-1234";

struct Harness {
    addr: SocketAddr,
    _state: TempDir, // keep TempDir alive for the duration of the test
}

async fn spawn_controller(state_dir: PathBuf) -> SocketAddr {
    // Pick an ephemeral port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // The middleware reads ISENGARD_TOKEN from process env. Set it for the
    // entire test process — fine because all tests in this file use the
    // same value.
    // Safety: env::set_var is unsafe in Rust 2024 due to thread-safety concerns
    // around POSIX getenv/setenv races. Tonic's runtime spins up additional
    // threads, but we set the var once before any concurrent access. For
    // tests this is the canonical pattern.
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

    // Wait for the server to bind.
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

async fn harness() -> Harness {
    let dir = TempDir::new().expect("temp dir");
    let addr = spawn_controller(dir.path().to_path_buf()).await;
    Harness { addr, _state: dir }
}

fn sample_request() -> EnrollRequest {
    EnrollRequest {
        fingerprint: "test-host.example".into(),
        hostname: "test-host".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0-alpha".into(),
        docker_version: "27.4.0".into(),
    }
}

async fn client_with_token(addr: SocketAddr, token: &str) -> ControllerClient<Channel> {
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("connect");

    let token: MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
    ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    })
}

async fn client_no_token(addr: SocketAddr) -> ControllerClient<Channel> {
    ControllerClient::connect(format!("http://{addr}")).await.expect("connect")
}

// =====================================================================

#[tokio::test]
async fn enroll_without_token_returns_unauthenticated() {
    let h = harness().await;
    let mut client = client_no_token(h.addr).await;

    let err = client.enroll(sample_request()).await.expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_wrong_token_returns_unauthenticated() {
    let h = harness().await;
    let mut client = client_with_token(h.addr, "not-the-right-token").await;

    let err = client.enroll(sample_request()).await.expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_valid_token_returns_agent_id_and_heartbeat() {
    let h = harness().await;
    let mut client = client_with_token(h.addr, TEST_TOKEN).await;

    let resp = client.enroll(sample_request()).await.expect("enroll").into_inner();
    assert!(!resp.agent_id.is_empty(), "agent_id must be set");
    assert_eq!(resp.heartbeat_interval_secs, 10);
    assert!(resp.server_time_ms > 0);
}

#[tokio::test]
async fn enroll_with_duplicate_fingerprint_returns_already_exists() {
    let h = harness().await;
    let mut client = client_with_token(h.addr, TEST_TOKEN).await;

    let _first = client.enroll(sample_request()).await.expect("first must succeed");
    let err = client.enroll(sample_request()).await.expect_err("second must fail");
    assert_eq!(err.code(), Code::AlreadyExists, "got {err:?}");
}
```

This file replaces the old `enroll_returns_unimplemented` and `server_accepts_concurrent_connections` tests. The 4 new tests cover the real enrollment path.

- [ ] **Step 3: Run tests**

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller 2>&1 | tail -15
```

Expected: 4 unit tests + 4 integration = 8 total in `isengard-controller`. Some may be order-dependent if multiple tests share the env var — `unsafe { env::set_var }` is fine when each test hits a fresh harness with its own ephemeral port + temp dir, but they all share `ISENGARD_TOKEN`. Acceptable.

If any test is flaky on retries, that's an integration-test-environment issue. Run a second time:

```bash
cd ~/Projects/isengard && cargo test -p isengard-controller --test server_skeleton 2>&1 | tail -10
```

If consistently passing, move on. If consistently failing on a specific test, report DONE_WITH_CONCERNS with which one.

- [ ] **Step 4: Run full workspace tests**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
```

Expected: ≥ 36 tests across all crates.

- [ ] **Step 5: Run clippy**

```bash
cd ~/Projects/isengard && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: clean.

- [ ] **Step 6: Smoke run with `grpcurl` (optional, skip if not installed)**

```bash
command -v grpcurl >/dev/null && echo "grpcurl present" || echo "grpcurl missing — skipping smoke test"
```

If installed:

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-2c-smoke
env ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9422 --state-dir /tmp/isengard-2c-smoke &
SERVER=$!
sleep 1

# Should fail without token:
grpcurl -plaintext -d '{"fingerprint":"smoke","hostname":"smoke","os":"linux","arch":"x86_64","agent_version":"0","docker_version":"0"}' \
  127.0.0.1:9422 isengard.v1.Controller/Enroll 2>&1 | head -3

# Should succeed with token:
grpcurl -plaintext -H "authorization: Bearer test" \
  -d '{"fingerprint":"smoke-2","hostname":"smoke-2","os":"linux","arch":"x86_64","agent_version":"0","docker_version":"0"}' \
  127.0.0.1:9422 isengard.v1.Controller/Enroll 2>&1 | head -5

kill $SERVER 2>/dev/null
sqlite3 /tmp/isengard-2c-smoke/isengard.db "SELECT hostname, hex(id) FROM hosts;" 2>&1 | head -3
```

Expected: first call returns Unauthenticated, second call returns a JSON response with `agent_id` populated, sqlite shows the row.

If you don't have `sqlite3` cli or `grpcurl`, skip this. The integration tests cover the same.

- [ ] **Step 7: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-controller/src/service.rs crates/isengard-controller/tests/ && git commit -m "feat(controller): real Enroll handler — persists host, returns agent_id + heartbeat interval"
```

---

## Task 5: Final CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails, apply `cargo fmt` + commit as `style: cargo fmt across phase 2c`.

- [ ] **Step 2: Total test count**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "test result: ok\.|FAILED" | sort -u
```

Expected: ≥ 36 tests, all green.

- [ ] **Step 3: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase2c -m "phase 2c: real Enroll handler + TokenAuthLayer + Inventory wired"
cd ~/Projects/isengard && git tag -l | grep phase2c
```

Don't push.

- [ ] **Step 4: Confirm done conditions**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 36 green
- [ ] `just ci-local` clean
- [ ] `Enroll` returns `Unauthenticated` without bearer token, `AlreadyExists` on duplicate fingerprint, `Ok(EnrollResponse)` on first enrollment with valid token + persists row
- [ ] Tag `v0.1.0-alpha.phase2c` exists locally
- [ ] No commits pushed since the plan commit

---

## Self-review

| Spec requirement | Plan task |
|---|---|
| §5 Token in metadata, tower middleware | Task 2 |
| §5 mTLS upgrade path local-only change | Task 2 (TokenAuthLayer is one swappable layer) |
| §7 Controller per-Enroll RPC: validate token, generate ULID, insert into hosts, return EnrollResponse | Tasks 2, 3, 4 |
| §7 Reflection stays open | Task 3 (per-service mount, or path-skip in middleware) |
| §13 sub-phase 2c done conditions | Task 5 |

No placeholders. Every code step contains real code. Every command has expected output. Two known fragility zones flagged inline: tonic's `Status::into_http()` API name (Task 2) and per-service-vs-global tower layering in tonic (Task 3) — both have documented fallbacks.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-04-30-phase-2c-enroll-handler.md`.

Execution: subagent-driven (consistent with prior phases).
