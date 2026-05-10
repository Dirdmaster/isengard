# Wisp Phase 0.4: agent integration (design)

Status: proposed
Phase: v0.4 foundation, wisp 0.4
Author: 2026-05-10
Depends on: wisp 0.1 (#127), wisp 0.2 (#128), wisp 0.3 (#129)

## Problem

Phases 0.1 + 0.2 + 0.3 of wisp built a working OCI runtime, image puller, and networking primitives. The runtime is sound (busybox + alpine + nginx demos all green). But isengard-agent still talks to dockerd via `bollard`. Until the agent uses wisp directly, all the work above is shelf-ware: real homelab fleets keep depending on the docker daemon.

Phase 0.4 makes wisp a real backend for the agent, behind an opt-in feature flag so the existing dockerd flow keeps working unchanged. The operator picks which runtime per host via env var.

## Goal

Land a `RuntimeBackend` trait in `isengard-agent` and two concrete impls: `BollardBackend` (existing behavior) and `WispBackend` (new, calls wisp + wisp-image + wisp-net). Selection by `ISENGARD_RUNTIME=docker|wisp` env var, default `docker` so existing operators see no change.

Phase 0.4 done bar:
- Existing dockerd-based fleets (lausanne, indra) keep running unchanged. `ISENGARD_RUNTIME` unset = bollard. All v0.3 integration tests pass.
- A new agent on a fresh OrbStack VM with `ISENGARD_RUNTIME=wisp` runs the v0.3 servarr-style stack (one of the migrated homelab compose files): controller pushes the compose, agent pulls the images via wisp-image, wires the network via wisp-net, runs the containers via wisp-runtime. Routing rules surface to Pingora the same way they do under bollard. `isd ps` shows the wisp-managed stack.
- Live cutover plan exists for migrating lausanne from bollard to wisp without losing operator visibility (drain old containers, flip the env var, recreate containers under wisp).

## Non-goals (Phase 0.4)

- Cross-host overlay networking (still tailscale-via-existing-adapter; cross-host wisp-net is post-0.5).
- bollard removal. Bollard stays as the default and as the fallback.
- Image GC scheduler. Manual `wisp image gc` for 0.4; auto-GC in 0.5.
- Compose `build:` directive. Operator builds elsewhere; wisp pulls.
- `docker exec`-style operator subcommand. Stretch: `isd exec` via wisp's deferred-from-0.1 `Runtime::exec` stretch goal.
- Resource limits beyond what 0.1 implements (memory/cpu/pids). swap, blkio, devices come later if real workloads need them.
- Automatic migration of existing dockerd containers into wisp on the same host. Migration is an explicit operator action: `isd stack down <stack>` (under bollard), flip the env var, `isd stack up <stack>` (under wisp).

## Design

### RuntimeBackend trait

The agent's bollard surface is finite. Inventory (today):
- Read: `list_containers`, `inspect_container`, `logs`, `events`.
- Write: `create_image` (pull), `create_container`, `start_container`, `stop_container`, `remove_container`, `connect_network`, `disconnect_network`.

Plus a few networking-adjacent ops that today happen via `connect/disconnect_network` and via the install-time `docker network create isengard-proxy`.

The trait abstracts these into wisp-vs-bollard-agnostic shapes.

```rust
// crates/isengard-agent/src/runtime/mod.rs
#[async_trait::async_trait]
pub trait RuntimeBackend: Send + Sync {
    /// Pull image if missing. Returns the manifest digest.
    async fn ensure_image(&self, reference: &str) -> Result<String, RuntimeError>;

    /// Create a container from the resolved bundle (image + spec).
    /// Returns an opaque container handle (the bollard impl uses container ID;
    /// wisp uses its own ID). The agent treats it as a String.
    async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String, RuntimeError>;

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError>;
    async fn stop_container(&self, id: &str, timeout_s: u32) -> Result<(), RuntimeError>;
    async fn remove_container(&self, id: &str, force: bool) -> Result<(), RuntimeError>;

    async fn list_containers(&self, filter: ListFilter) -> Result<Vec<ContainerSnapshot>, RuntimeError>;
    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerSnapshot>, RuntimeError>;

    async fn connect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError>;
    async fn disconnect_network(&self, container_id: &str, network: &str) -> Result<(), RuntimeError>;

    /// Stream container logs. Backend-specific stream type wrapped in a
    /// `Pin<Box<dyn Stream>>` for caller use.
    fn stream_logs(&self, id: &str, opts: LogOptions) -> Pin<Box<dyn Stream<Item = LogChunk> + Send>>;

    /// Stream runtime events (container.start, container.die, etc).
    fn stream_events(&self) -> Pin<Box<dyn Stream<Item = RuntimeEvent> + Send>>;

    /// Backend identity for diagnostics (`docker` or `wisp`).
    fn name(&self) -> &'static str;
}
```

`ContainerCreateSpec` is the agent's existing parsed-compose representation, modulo the bollard-specific bits. Most of `compose_apply.rs` already builds a `Config<String>` for bollard; we extract the higher-level intent (image, command, env, mounts, ports, networks, labels, restart policy) into a backend-agnostic struct, and each backend translates to its native shape.

### `ContainerCreateSpec`

```rust
pub struct ContainerCreateSpec {
    pub container_name: String,
    pub image: String,
    pub stack: String,
    pub service: String,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub mounts: Vec<MountSpec>,
    pub ports: Vec<PortSpec>,
    pub networks: Vec<String>,
    pub restart: RestartPolicy,
    pub healthcheck: Option<HealthcheckSpec>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
    pub hostname: Option<String>,
    pub linux_resources: Option<LinuxResources>,
    pub secrets: Vec<SecretMount>,
}
```

Today most of this is implicitly represented by `bollard::container::Config`. Phase 0.4 surfaces it explicitly.

### Backend selection

```rust
// crates/isengard-agent/src/runtime/select.rs
pub fn select_backend() -> Result<Arc<dyn RuntimeBackend>, RuntimeError> {
    match std::env::var("ISENGARD_RUNTIME").as_deref() {
        Ok("wisp") => Ok(Arc::new(WispBackend::from_env()?)),
        Ok("docker") | Err(_) => Ok(Arc::new(BollardBackend::from_env()?)),
        Ok(other) => Err(RuntimeError::UnknownBackend(other.into())),
    }
}
```

Logged once at agent startup: `runtime backend: docker` or `runtime backend: wisp`. `isd ps` and the dashboard show the backend name per host.

### BollardBackend

A thin wrapper around the existing code in `compose_apply.rs`, `compose_reconciler.rs`, `container_snapshot.rs`, `logs.rs`, `deployment/driver.rs`. We refactor to extract the raw bollard call sites behind the trait. No behavior change for existing dockerd-based deployments.

### WispBackend

```rust
pub struct WispBackend {
    runtime: Arc<wisp::Runtime>,
    image_client: Arc<wisp_image::Client>,
    net_attacher: Arc<dyn wisp::NetworkAttacher>,
    state_dir: PathBuf,
}
```

Method-by-method translation:

- `ensure_image(ref)` -> `image_client.pull(&ref.parse()?)`. Returns `pulled.manifest_digest`.
- `create_container(spec)`:
  1. `image_client.lookup(image)?` -> PulledImage.
  2. mkdir `<state_dir>/bundles/<container-name>/`.
  3. `BundleBuilder::new(&pulled, image_client.store(), &bundle_dir).assemble_rootfs()`.
  4. `BundleBuilder::write_config(ConfigOverrides::from_spec(spec))`.
  5. Build NetworkSpec from `spec.networks` + `spec.ports`.
  6. `wisp::Runtime::create_with_network(container-name, &bundle_dir, network_spec)` (or just `create` if no networks declared).
  7. Return container-name as the handle.
- `start_container(id)` -> `runtime.start_with_attacher(id, attacher)` (or `start` if no network).
- `stop_container(id, timeout)` -> `runtime.kill(id, SIGTERM)` then poll state until Stopped or timeout, then `runtime.kill(id, SIGKILL)` if still running.
- `remove_container(id, force)` -> `runtime.delete_with_attacher(id, force, attacher)`. Drops the bundle dir + content-store ref + IPAM allocation.
- `list_containers(filter)` -> `runtime.list()` filtered by labels (label match implemented in the agent, not the runtime : wisp doesn't index by label).
- `inspect_container(id)` -> `runtime.state(id)`. Map to ContainerSnapshot using the persisted spec + the live state.
- `connect/disconnect_network` -> calls into wisp_net via the attacher trait. Note: wisp containers are network-attached at create-time; reconnecting a running container is a separate code path that re-enters the netns. 0.4 supports the create-time path; live reconnect is 0.5.
- `stream_logs(id)` -> stream from `<bundle_dir>/log` (the wisp-cli logs file) via inotify-tail. Stretch: pipe stdout/stderr through a host-side fifo for live streaming.
- `stream_events()` -> wisp doesn't have a native event bus yet. 0.4 polls `runtime.list()` every 2s and emits diff events. Native event bus is 0.5.

### State migration / coexistence

A single host runs ONE runtime backend at a time. Containers created under bollard aren't visible to wisp (different state-dir, no shared registry). Operator migration for an existing host:

1. `isd stack down <stack>` (under bollard) : drains containers, frees ports.
2. Edit `/etc/isengard/isengard.env`: add `ISENGARD_RUNTIME=wisp`.
3. `docker restart iso-agent` (the agent container itself still runs under bollard / its host's docker; wisp is for the workload, not for the agent).
4. `isd stack up <stack>` (under wisp) : wisp-image pulls, wisp-net wires, wisp-runtime runs.

Lifetime of bollard-managed containers + wisp-managed containers on the same host = mutually exclusive. The trait selection is global per agent process. Future: per-stack backend opt-in (operator labels a stack `isengard.runtime=wisp`); deferred to 0.5+.

### Agent container itself

The agent is currently shipped as a docker container that mounts `/var/run/docker.sock`. With ISENGARD_RUNTIME=wisp, the agent NO LONGER needs the docker socket : it talks to wisp directly via the local Linux ABI (clone3, /sys/fs/cgroup, etc).

Changes:
- Agent compose file gains a conditional: `if ISENGARD_RUNTIME=wisp: don't mount docker.sock`. install.sh updates accordingly.
- Agent must run with the right capabilities for wisp to work: SYS_ADMIN (mounts), NET_ADMIN (iptables, bridge), SYS_PTRACE (proc inspection), SYS_RESOURCE (cgroup writes). Run as root + `--cap-drop=ALL --cap-add=SYS_ADMIN,NET_ADMIN,SYS_PTRACE,SYS_RESOURCE` OR as `--privileged` for 0.4 (tighten later).
- Agent's state-dir bind-mount needs `/var/lib/wisp` and `/etc/isengard/stacks` AND access to `/sys/fs/cgroup/wisp` AND `/sys/class/net/`.

This is a non-trivial install-flow change. Document in `RELEASE_NOTES_WISP_PHASE_0_4.md`.

### Logs

Today's logs flow: `bollard::Docker::logs(container_id, follow=true)` returns a stream of `LogOutput` frames. The dashboard's logs WebSocket pumps these to the operator.

Wisp impl: the runtime's child writes to its inherited stdout/stderr. wisp-cli's foreground mode (non-detach) tees these to the parent terminal. For wisp-as-backend, we need a different approach: redirect the child's stdout/stderr to a file in `<state_dir>/containers/<id>/{stdout,stderr}.log` BEFORE exec, then `inotify-tail` those files for streaming.

This is an addition to wisp 0.1's lifecycle code. Spec it as a small wisp 0.5 patch (or fold into 0.4): in `lifecycle::start_container`, before `clone3`, open `<state_dir>/containers/<id>/{stdout,stderr}.log` files; after clone3, parent dups those fds into the child's namespace via `setns` + `dup2`. Or simpler: parent passes the fds through the sync pipe and the child dup2's before exec. The latter avoids a setns dance.

For Phase 0.4: implement the simpler "child dup2 stdout/stderr to file before exec" path inside `wisp` (a small change to `lifecycle/mod.rs`). The agent's `stream_logs` then inotify-tails the files.

### Events

Bollard exposes `Docker::events()` with `container.start`, `container.die`, `container.kill`, `container.health_status` etc. The agent uses these to detect container lifecycle changes (notify the controller, drive blue-green state machines, etc).

Wisp's runtime doesn't emit events natively. For 0.4, the WispBackend polls `runtime.list()` every 2s, diffs against the previous snapshot, and synthesizes events. Tracker: container appears = `start`, container disappears = `die`, container state transitions = state-change events.

This is good-enough for compose-reconciler use cases (which rely on healthchecks more than container.die). For policies that care about exact die timing (Phase 9 update policies), we accept ~2s skew. Document in release notes; tighten to fsnotify on the cgroup's `cgroup.events` file in 0.5.

### Healthchecks

Bollard runs healthchecks inside the container (`docker run --health-cmd=...`). Wisp 0.1-0.3 don't run healthchecks: the entrypoint is whatever the spec says, and that's it.

For 0.4: the agent runs healthchecks externally via the existing `proxy/health.rs` HTTP probe (which already does HTTP healthchecks for routing decisions). Compose-spec `healthcheck:` directives translate to: external HTTP probe if the test is `["CMD", "curl", "..."]`; otherwise, a one-shot `nsenter -t <pid> -- <test>` invocation against the container's namespaces.

Concretely add to RuntimeBackend:
```rust
async fn run_healthcheck(&self, id: &str, hc: &HealthcheckSpec) -> Result<HealthState, RuntimeError>;
```
Bollard impl uses Docker's built-in health. Wisp impl uses nsenter + the test command. Polled by the agent on the same cadence as today.

### Secrets

Phase 0.3.6's encrypted secrets store + tmpfs `/run/secrets/<name>` flow today goes through bollard's bind-mount. Wisp impl: same shape. The agent fetches secret bytes, writes to a tmpfs file, includes it in `ContainerCreateSpec.secrets`, and the WispBackend translates secrets -> bind-mounts in the synthesised OCI config.

No protocol change required.

## Test strategy

### Unit tests (Mac OK)

- `RuntimeBackend` trait shape + mock impl in `runtime/mock.rs` for testing the agent's reconciler against both backends without real syscalls.
- `select_backend` ENV parsing.
- `BollardBackend::create_container` builds the same `bollard::Config` as today's compose_apply (golden test: the existing test suite still passes with the refactor).
- `WispBackend::create_container` builds the right OCI bundle + NetworkSpec given a known ContainerCreateSpec.
- ConfigOverrides translation (image config + override merge).

### Integration tests (OrbStack VM as root)

- `agent_under_wisp.rs` (`#[ignore]`): spin up a mini-controller + agent (the existing test harness), run a hello-world stack, assert it reaches Running, healthcheck-passes, then tear down. Both with `ISENGARD_RUNTIME=docker` (regression) and `ISENGARD_RUNTIME=wisp` (new).
- `servarr_style_stack_under_wisp.rs` (`#[ignore]`): more realistic: a 3-container stack (postgres + web + nginx) on a wisp bridge. Assert inter-container DNS works (dispatch C of 0.3 wired this), nginx routes via Pingora's label-discovery, postgres accepts connections from web.

### Regression (existing tests)

The whole agent test suite must pass with `ISENGARD_RUNTIME` unset (default = docker). No change. The trait abstraction's only purpose at 0.4 is to add wisp; the bollard path is byte-identical.

### Demo

```
orb -m wisp-agent -u root bash         # a fresh VM, not the existing wisp dev VM

# Standard isengard install, but with ISENGARD_RUNTIME=wisp before bring_up
ISENGARD_RUNTIME=wisp bash -c \
  "$(curl -fsSL https://raw.githubusercontent.com/Weavers-Engineering/isengard/next/install/install.sh)"

# Operator from their Mac
isd context create vm-test --ssh dirdmaster@<vm-ip> --use
isd deploy ~/Projects/homelab/packages/infra/apps/hello/compose.yml
isd ps
# expected: hello stack running on the VM, backend=wisp
isd open hello
# expected: browser opens hello.local, hits Pingora, hits the wisp container
```

## Risks

- **Capability requirements.** The agent under wisp needs SYS_ADMIN + NET_ADMIN + SYS_PTRACE + SYS_RESOURCE at minimum. `--privileged` works but is broad. Tighten in 0.5.
- **OrbStack vs bare-metal.** OrbStack VMs work for wisp dev; bare-metal Linux is the production target. Validate on a non-OrbStack Linux host (DigitalOcean droplet, Hetzner, etc.) BEFORE recommending wisp for production homelabs.
- **Healthcheck skew.** External HTTP probes vs Docker's in-container healthcheck may behave differently for unusual `test:` shapes. Document supported test shapes.
- **Event polling skew.** 2s polling miss = 2s lag for Phase 9 policy decisions. Acceptable for 0.4; document.
- **Image-pull rate limits.** wisp-image pulls anonymously from Docker Hub (100/6h IP rate limit). Operator hits the limit -> `ensure_image` fails -> compose deploy fails. Document; auth comes in 0.5.
- **State-dir migration.** Going from bollard back to wisp on the same host requires manual `wisp image gc` cleanup of unused images. Not catastrophic but documentable.
- **Agent self-updater.** Phase 4's agent self-update flow uses bollard to recreate itself. Under wisp-as-backend, this needs a bootstrap path: the agent container itself is still bollard-managed (run by docker / orbstack / whatever the host has). Wisp manages the WORKLOAD containers. Document explicitly.

## Stretch goals (only if 0.4 lands fast)

- Per-stack runtime opt-in via compose label `isengard.runtime=wisp` (today: global per-agent).
- Live network reconfig (re-attach a running container to a new network). Today: recreate.
- `isd exec` via wisp's deferred `Runtime::exec`.
- Wisp event bus (fsnotify on cgroup.events) replacing the polling fallback.

## Open questions

- **State-dir for wisp**. Options: `/var/lib/isengard/wisp/` (under the existing isengard state dir, namespaced) vs `/var/lib/wisp/` (separate, matches the wisp-cli default). Default to nested: `/var/lib/isengard/wisp/`. Cleaner uninstall.
- **Bollard removal**. Long-term we want to drop the bollard dep entirely. 0.4 keeps both. 0.5 considers removing bollard if wisp is feature-complete + battle-tested. Phase 0.6 retires bollard if revenue / fleet adoption justifies.
- **Backend in `isd ps` output**. Add a "backend" column. Operator sees per-host runtime at a glance.
- **Telegraf / metrics**. Existing metrics (container counts, image pulls, etc.) are docker-flavored. Translate to backend-agnostic shapes. Phase 0.5 polish.
