# Wisp Phase 0.4 Agent Integration: Implementation Plan

> Spec: [`2026-05-10-wisp-phase-0-4-agent-integration-design.md`](../specs/2026-05-10-wisp-phase-0-4-agent-integration-design.md). Branch: `wisp/phase-0-4` (stacks on `wisp/phase-0-3`).

## Scope

Land `RuntimeBackend` trait inside `crates/isengard-agent/src/runtime/` plus two concrete impls (`BollardBackend`, `WispBackend`). Select via `ISENGARD_RUNTIME=docker|wisp` env var, default `docker`. Refactor compose_apply, compose_reconciler, container_snapshot, logs, deployment/driver to call through the trait instead of bollard directly. Done bar: existing fleets unaffected; fresh VM with `ISENGARD_RUNTIME=wisp` runs the servarr-style stack end-to-end.

## Dev environment

Two OrbStack VMs:
- `wisp` (existing): for wisp-runtime / wisp-image / wisp-net dev. Stays unchanged.
- `wisp-agent` (new for this phase): a fresh isengard-agent install with `ISENGARD_RUNTIME=wisp`. Tests the cutover path against a real controller.

## Files touched

| File | Change |
| --- | --- |
| `crates/isengard-agent/Cargo.toml` | add wisp + wisp-image + wisp-net path deps; add async-trait |
| `crates/isengard-agent/src/runtime/mod.rs` | new, RuntimeBackend trait + ContainerCreateSpec + RuntimeError |
| `crates/isengard-agent/src/runtime/select.rs` | new, env-driven backend factory |
| `crates/isengard-agent/src/runtime/bollard_backend.rs` | new, refactor of existing bollard calls |
| `crates/isengard-agent/src/runtime/wisp_backend.rs` | new, wisp + wisp-image + wisp-net translation |
| `crates/isengard-agent/src/runtime/spec.rs` | new, ContainerCreateSpec + supporting types |
| `crates/isengard-agent/src/compose_apply.rs` | refactor to call trait methods, not bollard directly |
| `crates/isengard-agent/src/compose_reconciler.rs` | similar refactor for snapshot reads |
| `crates/isengard-agent/src/container_snapshot.rs` | extract bollard inspect mapping into BollardBackend::inspect_container |
| `crates/isengard-agent/src/logs.rs` | abstract log streaming behind trait |
| `crates/isengard-agent/src/deployment/driver.rs` | image-pull through trait |
| `crates/isengard-agent/src/lib.rs` | wire backend at startup |
| `crates/wisp/src/lifecycle/mod.rs` | small extension: redirect child stdout/stderr to file before exec |
| `install/install.sh` | conditional docker.sock mount based on ISENGARD_RUNTIME |
| `install/compose.yaml` | document the wisp-mode capabilities |
| `crates/isengard-agent/tests/wisp_backend_smoke.rs` | new, ignored, root-only |
| `docs/RELEASE_NOTES_WISP_PHASE_0_4.md` | new |

## Steps

Four sequenced dispatches.

### Dispatch A: RuntimeBackend trait + BollardBackend (refactor)

Pure refactor + abstraction. NO new behavior. Existing tests must continue to pass.

#### A1: trait + types + RuntimeError

- `crates/isengard-agent/src/runtime/mod.rs`: define `RuntimeBackend` trait per the spec. `#[async_trait]`.
- `crates/isengard-agent/src/runtime/spec.rs`: define `ContainerCreateSpec`, `MountSpec`, `PortSpec`, `RestartPolicy`, `HealthcheckSpec`, `LinuxResources`, `SecretMount`, `LogOptions`, `LogChunk`, `RuntimeEvent`, `ContainerSnapshot`, `ListFilter`. These are backend-agnostic shapes.
- `crates/isengard-agent/src/runtime/error.rs`: `RuntimeError` enum (Network, Image, Container, Network, Backend, UnknownBackend, etc).
- Wire into `lib.rs` as `pub mod runtime;`.
- Tests: shape-level (a mock backend impl in the test module exercising the trait surface).
- Commit: `feat(agent): RuntimeBackend trait and ContainerCreateSpec types`

#### A2: select_backend()

- `crates/isengard-agent/src/runtime/select.rs`: env-driven factory per spec.
- Returns `Result<Arc<dyn RuntimeBackend>, RuntimeError>`.
- Tests: env parsing variations.
- Commit: `feat(agent): select_backend env factory`

#### A3: BollardBackend

- `crates/isengard-agent/src/runtime/bollard_backend.rs`: extract every `Docker::*` call from compose_apply, compose_reconciler, container_snapshot, logs, deployment/driver into trait method impls. Use the EXACT existing logic: this is a refactor, not a rewrite.
- ContainerCreateSpec -> `bollard::container::Config<String>` translation lives here.
- ContainerSnapshot mapping (from `bollard::secret::ContainerInspectResponse`) lives here.
- Logs stream via `bollard::Docker::logs` wrapped in the trait shape.
- Events stream via `bollard::Docker::events`.
- Tests: golden test that BollardBackend::create_container produces the same `bollard::Config` as today's compose_apply for the busybox demo, the servarr stack, etc.
- Commit: `feat(agent): BollardBackend extracts bollard surface behind trait`

#### A4: refactor call sites

- compose_apply, compose_reconciler, container_snapshot, logs, deployment/driver: replace direct `Docker::*` calls with `backend.method(...)`.
- The `Arc<bollard::Docker>` field on the agent's main loop becomes `Arc<dyn RuntimeBackend>`.
- `BollardBackend::from_env()` is the only place that constructs the `bollard::Docker` handle.
- All existing agent tests must pass without modification.
- Commit: `refactor(agent): wire RuntimeBackend trait through compose / logs / deploy paths`

Validate: `cargo test -p isengard-agent`, integration tests on the existing wisp VM (no behavior change), production smoke against indra (operator-driven).

### Dispatch B: WispBackend

#### B1: ContainerCreateSpec -> ConfigOverrides translation

- `crates/isengard-agent/src/runtime/wisp_backend.rs`: add `ConfigOverrides::from_spec(spec)` helper (or place in `runtime/spec.rs`) that converts ContainerCreateSpec to wisp_image::ConfigOverrides.
- Also `NetworkSpec::from_spec(spec)` for the network attachment shape.
- Tests: golden translation tests.
- Commit: `feat(agent): ConfigOverrides translation from ContainerCreateSpec`

#### B2: WispBackend impl (CRUD)

- `WispBackend::new(state_dir)` constructs `wisp::Runtime`, `wisp_image::Client`, and the net attacher.
- `ensure_image(ref)` -> `image_client.pull(&ref.parse()?)`; returns manifest_digest.
- `create_container(spec)`:
  1. lookup or pull the image
  2. mkdir bundle dir under `<state_dir>/bundles/<container-name>/`
  3. BundleBuilder::assemble_rootfs + write_config
  4. compose NetworkSpec; call Runtime::create_with_network or Runtime::create
  5. return container-name (the handle)
- `start_container`, `stop_container` (SIGTERM then poll then SIGKILL), `remove_container` (delete + free bundle + drop ref + free IPAM).
- `list_containers(filter)` -> `runtime.list()` + label filter applied in the agent.
- `inspect_container(id)` -> `runtime.state(id)` mapped to ContainerSnapshot using the persisted ContainerCreateSpec.
- `connect/disconnect_network` -> attacher trait calls.
- Persist ContainerCreateSpec alongside the wisp state-dir entry so inspect can reconstruct snapshots after restart.
- Tests: round-trip tests against tempdir + fake wisp::Runtime where useful.
- Commit: `feat(agent): WispBackend CRUD + image / network plumbing`

#### B3: Healthchecks (run_healthcheck)

- BollardBackend::run_healthcheck uses Docker's built-in health.
- WispBackend::run_healthcheck:
  - HTTP test (e.g. `["CMD-SHELL", "curl http://localhost:80/"]`): use the agent's existing proxy/health.rs HTTP probe directly.
  - Other shapes: `nsenter -t <pid> -- <test args>` (via wisp_net::ns::run_in_netns when relevant, or a thin nsenter wrapper for non-network namespaces).
- Tests: hermetic against a busybox bundle that exits 0 / exits 1.
- Commit: `feat(agent): WispBackend run_healthcheck (HTTP + nsenter)`

### Dispatch C: logs + events for WispBackend

#### C1: wisp lifecycle stdout/stderr -> file

- Modify `crates/wisp/src/lifecycle/mod.rs`: before clone3, parent opens `<state_dir>/containers/<id>/{stdout,stderr}.log` files. Pass the fds into the child (via the existing sync pipe). Child dup2's them to fd 1 + 2 BEFORE exec.
- Adds two new fields to ContainerHandle (or to NetworkAttachmentRecord): stdout_log_path, stderr_log_path.
- Update existing wisp integration tests where they assume stdout went to the parent terminal.
- The busybox demo continues to work because anyhow / println! still go through fd 1 + 2 which now points at the log file. The example's `cargo run` behavior changes (output goes to file, not terminal); the example wrapper reads + prints the log content for parity.
- Commit (in wisp crate, will need to land alongside this PR): `feat(wisp): redirect child stdout/stderr to log files`

#### C2: WispBackend stream_logs

- Use `inotify` (via the `notify` crate already in the workspace) to watch the log files. Stream LogOutput frames through the trait's stream type.
- Backfill: read the existing file content first, then watch for new appends.
- Bollard's logs stream returns `LogOutput { Console | StdErr | StdOut | StdIn }`. Map the wisp file shape to the same enum (split by source: stdout.log -> StdOut, stderr.log -> StdErr).
- Commit: `feat(agent): WispBackend log streaming via inotify`

#### C3: WispBackend stream_events (poll + diff)

- Background tokio task polling `runtime.list()` every 2s, diffing against previous snapshot, emitting RuntimeEvent { container_id, event_type } via a tokio broadcast channel that stream_events subscribes to.
- Event types: ContainerStart, ContainerStop, ContainerDie, HealthcheckPassed, HealthcheckFailed.
- Tests: simulate state transitions via the mock runtime; assert correct events.
- Commit: `feat(agent): WispBackend event polling + diff emitter`

### Dispatch D: install flow + integration tests + demo + release notes

#### D1: install.sh + compose.yaml updates

- `install/install.sh`: conditional based on `ISENGARD_RUNTIME` env (set before invocation):
  - if `wisp`: don't mount docker.sock; ensure `/sys/fs/cgroup` is writable; pass `--privileged` (or the cap set) to the agent container.
  - if `docker` or unset: existing behavior.
- `install/compose.yaml`: same conditional shape.
- Document operator capabilities at install time.
- Commit: `chore(install): conditional docker.sock mount based on ISENGARD_RUNTIME`

#### D2: integration tests

- `crates/isengard-agent/tests/wisp_backend_smoke.rs`, `#[ignore]`, root-only:
  - Bring up a controller + agent inside a fresh OrbStack VM with `ISENGARD_RUNTIME=wisp`.
  - Push a hello-world compose (single nginx container, port published).
  - Wait for the agent to pull the image, create the bundle, run the container, attach the network.
  - Assert: `isd ps` shows the stack; `curl <ip>:<port>` returns the nginx page.
  - Tear down.
- `crates/isengard-agent/tests/bollard_backend_regression.rs`: parallel test under the default backend (existing logic). Same hello-world stack. Asserts no regression.
- Commit: `test(agent): wisp backend smoke + bollard regression`

#### D3: demo + release notes

- Bring up a fresh OrbStack VM `wisp-agent` with `ISENGARD_RUNTIME=wisp` set before install. Capture verbatim demo output.
- `docs/RELEASE_NOTES_WISP_PHASE_0_4.md` per the standard pattern.
- Commit: `docs(wisp): phase 0.4 release notes + integration demo`

## Validation

Per-dispatch:
- `cargo test --workspace --lib`
- `cargo test --workspace --tests`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo deny check`

After dispatch A: existing agent tests must pass without modification (refactor is by definition behavior-preserving). Run on the existing test infrastructure.

After dispatch D: end-to-end demo on a fresh VM. The hello-world stack flow must work.

## Risks

Spec calls them out; reproduce here:
- Capability requirements (SYS_ADMIN, NET_ADMIN, SYS_PTRACE, SYS_RESOURCE).
- OrbStack vs bare-metal.
- Healthcheck skew between bollard's in-container probe and wisp's external probe.
- Event polling 2s skew.
- Image pull rate limits (anonymous Docker Hub).
- Agent self-update flow (the agent container itself runs under the host's docker, not under wisp).

## Dispatch sequence

| Dispatch | Steps | Notes |
|---|---|---|
| A | A1 / A2 / A3 / A4 | Pure refactor. Existing tests must still pass. |
| B | B1 / B2 / B3 | New WispBackend functionality. |
| C | C1 / C2 / C3 | Logs + events. C1 modifies wisp crate; coordinate with the existing wisp PRs. |
| D | D1 / D2 / D3 | Install flow + integration tests + demo + release notes. End-to-end smoke gates the dispatch. |

## Open questions during implementation

- **Where does WispBackend persist ContainerCreateSpec?** Options: `<state_dir>/containers/<id>/spec.json` (next to the wisp state) vs `<state_dir>/bundles/<id>/spec.json` (next to the bundle). Pick whichever survives wisp's own state-dir conventions cleanly.
- **Does the agent run wisp::Runtime in the agent's tokio context, or in a dedicated single-threaded task?** clone3 needs single-threaded. Suggestion: spawn a dedicated `tokio::task::spawn_blocking` for each Runtime::start call. Decision in B2.
- **Do we need `cargo run -p wisp-cli` on the agent host?** No : the agent links wisp directly. wisp-cli is for operator debugging only.
- **Stack-level migration semantics.** What if a stack has 5 services and the operator switches mid-stack? Today: full stack down, switch, full stack up. Per-service runtime opt-in is a 0.5 stretch.
