# Wisp Phase 0.4: agent integration

> Phase 0.4 of wisp. Branch: `wisp/phase-0-4`. Builds on 0.1's runtime + 0.2's image pulling + 0.3's networking; not in `next` yet, no operator-facing change for v0.3.x users.

## What this is

`crates/isengard-agent/src/runtime/` is the agent's new backend abstraction. A `RuntimeBackend` trait wraps every runtime call the agent makes (image pull, container CRUD, log streaming, event streaming, healthchecks); two concrete impls slot in behind it: `BollardBackend` (existing dockerd flow, byte-identical behavior) and `WispBackend` (new, drives `wisp` + `wisp-image` + `wisp-net` directly).

Selection is per-host via env var:

```sh
ISENGARD_RUNTIME=docker  # default; existing behavior, unchanged
ISENGARD_RUNTIME=wisp    # opt-in; agent talks to wisp directly, NOT to dockerd
```

Set on the agent host before bringing up the stack. The agent logs the choice once at startup (`runtime backend: docker (bollard)` or `runtime backend: wisp`) so the operator can verify per host.

This is the fourth of four foundation phases on the way to v0.4: 0.1 was the runc-equivalent runtime, 0.2 was image pulling, 0.3 was networking, 0.4 wires it into the agent. With 0.4 landed, an operator can run an isengard agent on a fresh host with no docker daemon at all (the agent container itself still runs under the host's docker; only the workload containers move).

## What's NOT in 0.4

- **Auto image GC.** wisp-image's content store accumulates layers across pulls. `wisp image gc` (CLI, manual) works; the agent does not schedule automatic cleanup. Tracked for 0.5.
- **Cross-host overlay networking.** `wisp-net` remains single-host. Cross-host fabric continues to ride the agent's existing tailscale + docker-network adapter.
- **Live network reconfig.** Adding a port / network to a running container is delete + recreate. wisp attaches the network once between clone3 and execve.
- **`--network host` / `--network none`.** Default is no-network; explicit `--network <name>` opts in. Host / none come post-0.5.
- **Per-stack runtime opt-in.** `ISENGARD_RUNTIME` is global per agent process. A future label-driven mode (`isengard.runtime=wisp` on a stack) is a 0.5 stretch.
- **`isd exec`.** wisp's `Runtime::exec` was a 0.1 stretch goal that did not land. Operator-driven shell-into-container is a 0.5 follow-up.
- **Native event bus.** wisp emits no events of its own; the WispBackend polls `runtime.list()` every 2s and synthesises Start / Die / Stop events from the diff. Acceptable skew for compose-reconciler / blue-green policies (which lean on healthchecks more than container.die timing). `cgroup.events` fsnotify is 0.5.
- **Bollard removal.** Bollard stays as the default and as the fallback. Removing the dependency outright is a 0.6+ decision once wisp has fleet hours.

## Done bar

The `WispBackend` smoke test (`crates/isengard-agent/tests/wisp_backend_smoke.rs`) drives the full lifecycle (pull, create, start, inspect, remove) against a real busybox image, on a real cgroup v2 host, on the OrbStack `wisp` VM (Ubuntu noble, kernel 6.x, arm64) as root.

Verbatim output from a clean VM run:

```text
$ orb -m wisp -u root bash
$ PATH=/home/dirdmaster/.cargo/bin:$PATH
$ cd /Users/dirdmaster/Projects/isengard/.worktrees/next
$ CARGO_TARGET_DIR=/root/wisp-target cargo test -p isengard-agent \
    --test wisp_backend_smoke -- --ignored --nocapture wisp_backend_busybox_lifecycle

   Compiling isengard-agent v0.1.0-alpha (...)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 5.97s
     Running tests/wisp_backend_smoke.rs (...)

running 1 test
test wisp_backend_busybox_lifecycle ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.02s
```

What the test exercises in order:
1. `WispBackend::from_env(state_dir)` boots, mkdir's `<state_dir>/{wisp,bundles,images,spec,networks}`, attaches to `/sys/fs/cgroup/wisp`, spawns the C3 event-emitter loop.
2. `ensure_image("docker.io/library/busybox:latest")` pulls + caches the image, returns its manifest digest.
3. `create_container(spec)` looks up the image, mkdir's `<state_dir>/bundles/wisp-smoke-1/`, calls `BundleBuilder::assemble_rootfs` + `write_config`, persists `<state_dir>/spec/wisp-smoke-1/spec.json`, calls `wisp::Runtime::create`.
4. `start_container(id)` clone3's the busybox process under `/sys/fs/cgroup/wisp/wisp-smoke-1/` (no network attached, hermetic), the child runs `echo wisp-smoke-ok && sleep 3 && exit 0`.
5. `inspect_container(id)` reaches `ContainerState::Exited` within 10s; the agent reads back the persisted spec to populate labels / stack / service.
6. `stream_events()` delivers a `Start` event (synchronously emitted by `start_container`); `Die` is best-effort with the 2s emitter poll (often emitted, occasionally missed for fast-exit containers).
7. `remove_container(id, force=true)` calls `Runtime::delete`, drops the bundle dir, removes the persisted spec, frees the IPAM allocation.

## Bollard regression

`crates/isengard-agent/tests/bollard_backend_regression.rs` runs the same hello-world (`echo bollard-regression-ok`) against the default `BollardBackend` so we can prove the trait wiring did not regress dockerd. Mac side, OrbStack docker engine:

```text
$ cargo test -p isengard-agent --test bollard_backend_regression -- --ignored --nocapture

running 1 test
test bollard_backend_busybox_lifecycle ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.66s
```

Existing `cargo test -p isengard-agent --lib` remains 202 passing, 1 ignored.

## Public API

```rust
use isengard_agent::runtime::{
    select_backend, ContainerCreateSpec, RestartPolicy, RuntimeBackend,
};
use std::path::PathBuf;

let state_dir = PathBuf::from("/var/lib/isengard/agent");
let backend: std::sync::Arc<dyn RuntimeBackend> =
    select_backend(&state_dir).await?;

// Backend logs its own startup line:
//   "runtime backend: docker (bollard)"  or  "runtime backend: wisp"

let digest = backend.ensure_image("docker.io/library/busybox:latest").await?;
let id = backend.create_container(&spec).await?;
backend.start_container(&id).await?;
// ...
backend.remove_container(&id, true).await?;
```

`select_backend` reads `ISENGARD_RUNTIME` once. Unset / empty / `docker` (case-insensitive) returns a `BollardBackend`; `wisp` returns a `WispBackend`; anything else errors with `RuntimeError::UnknownBackend`.

## Capabilities required (wisp mode)

The agent under wisp needs broad kernel privileges. Phase 0.4 grants `--privileged` for simplicity; the cap-set drop-down is tracked for 0.5.

| Capability | Why |
|---|---|
| `SYS_ADMIN` | mount(2), pivot_root, namespace ops for clone3 |
| `NET_ADMIN` | iptables, bridge, veth, sysctl `net.ipv4.*` |
| `SYS_PTRACE` | `/proc/<pid>/ns/*` access for nsenter healthchecks |
| `SYS_RESOURCE` | cgroup v2 writes outside the agent's own slice |

`install/compose.wisp.yaml` ships with `privileged: true`; the operator-facing TODO inside that file explains the cap-add route.

## Install flow

`ISENGARD_RUNTIME=wisp` set before `install.sh` switches the rendered compose:

```sh
export ISENGARD_RUNTIME=wisp
sudo -E bash install/install.sh
```

Differences from the default install:

| Concern | Default (docker) | Wisp |
|---|---|---|
| Compose source | `install/compose.yaml` | `install/compose.wisp.yaml` |
| Agent docker.sock mount | yes | NO |
| Agent capabilities | none extra | `--privileged` (Phase 0.4) |
| Agent extra mounts | none | `/sys/fs/cgroup:rw`, `/var/lib/wisp:rw` |
| Agent env | (default) | `ISENGARD_RUNTIME=wisp` |
| Workload runtime | bollard -> dockerd | wisp directly |

Verified end-to-end on the OrbStack `wisp` VM: `bash install/install.sh` with `ISENGARD_RUNTIME=wisp ISENGARD_SKIP_BRING_UP=1` writes `compose.yaml` containing `privileged: true`, `/sys/fs/cgroup:/sys/fs/cgroup`, `/var/lib/wisp:/var/lib/wisp`, `ISENGARD_RUNTIME: wisp`, and NO `docker.sock` bind line. Snippet of the rendered file (selected lines):

```text
86:    privileged: true
106:      - ${ISENGARD_WISP_HOST_DIR:-/var/lib/wisp}:/var/lib/wisp:rw
110:      - /sys/fs/cgroup:/sys/fs/cgroup:rw
117:      ISENGARD_RUNTIME: wisp
```

## Migration: dockerd to wisp

Single host, single backend at a time. Containers created under bollard are not visible to wisp (different state-dir, no shared registry). Operator path:

1. `isd stack down <stack>` for every stack, against the running bollard-backed agent. Drains containers + frees ports + releases proxy routes.
2. Set `ISENGARD_RUNTIME=wisp` in the host environment.
3. Re-run `sudo -E bash install/install.sh` and pick the `Refresh compose.yaml only` reinstall menu option. install.sh swaps `compose.yaml` for the wisp variant, preserves master.key + secrets DB + isengard.env, recreates the controller + agent containers.
4. `isd stack up <stack>` per stack. WispBackend pulls images via `wisp-image`, mkdir's bundles, clone3's the workload.

Going back to docker is the same flow with `ISENGARD_RUNTIME=docker` (or unset) before the install. Wisp's content store at `/var/lib/isengard/agent/images/` is left in place; manual `wisp image gc` reclaims the space.

## Disk layout (wisp mode)

```text
/var/lib/isengard/agent/         # agent's state-dir (compose.wisp.yaml mounts here)
  wisp/                          # wisp::Runtime: state.json + cgroup handles
    containers/<id>/state.json
    containers/<id>/{stdout,stderr}.log
  bundles/<id>/                  # OCI bundles assembled per container
  images/                        # wisp-image content store
  spec/<id>/spec.json            # WispBackend: persisted ContainerCreateSpec
  networks/<name>/allocs.json    # wisp-net IPAM bitmap

/sys/fs/cgroup/wisp/             # cgroup v2 hierarchy, owned by the agent
  <container-id>/cgroup.procs
  <container-id>/memory.max
  ...

/var/lib/wisp/                   # operator-debugging path (compose.wisp.yaml mounts);
                                 # not the agent's primary state, but reserved for
                                 # `wisp ls` from the host.
```

The "nested under the agent's state-dir" choice (vs the `/var/lib/wisp` path that `wisp-cli` defaults to) was made for cleaner uninstall: wiping `/var/lib/isengard/` removes everything wisp owns. Open spec question, decided here.

## Known limitations

- **`--privileged` for 0.4.** A tighter cap set (`SYS_ADMIN + NET_ADMIN + SYS_PTRACE + SYS_RESOURCE`, no `--privileged`) needs validation against AppArmor / SELinux profiles before promotion. Phase 0.5.
- **Event polling skew.** The 2s `runtime.list()` diff loop can miss containers that exit faster than one tick. Smoke-test workaround: `sleep 3` in the busybox command. Production impact: deployment driver / blue-green policies see a ~2s lag on Die. Tighten with `cgroup.events` fsnotify in 0.5.
- **No native healthcheck inside the container.** wisp containers don't run an in-container probe (Docker's `--health-cmd` model). The WispBackend impl runs healthchecks externally: HTTP probes via the agent's `proxy/health.rs` HTTP client, other shapes via `nsenter -t <pid> -- <test>`. Supported test shapes are documented in the spec; arbitrary host-side commands work, exotic Docker `HEALTHCHECK` shapes may not.
- **Image pull rate limits.** wisp-image pulls anonymously from Docker Hub (100 / 6h IP rate limit). Hitting the limit -> `ensure_image` fails -> compose deploy fails. Authentication is 0.5.
- **Default network attacher needs operator pre-creation.** `WispBackend::from_env` lazily constructs a `WispNetAttacher` for the network name `default` (subnet from `WISP_DEFAULT_SUBNET`, fallback `10.83.0.0/24`). Containers that declare `networks: [...]` in their spec hit this attacher; containers without networks use the no-net `Runtime::start` path. The smoke test runs the no-net path. Networked end-to-end is gated on operator-pre-created bridges; auto-create-on-demand is 0.5.
- **Agent self-update.** Phase 4's agent self-update path uses bollard to recreate the agent container against the host's docker. Under wisp-as-backend, the agent container itself still runs under the host's docker (compose.wisp.yaml is a docker-compose file). Wisp manages the WORKLOAD; the agent's own container is bollard-managed. This is by design and documented here so operators don't try to run "everything under wisp" for v0.4.
- **OrbStack vs bare-metal.** All Linux integration was on OrbStack VMs (Ubuntu noble, kernel 6.x, arm64, cgroup v2). Bare-metal Linux (Hetzner, DigitalOcean, RPi) needs its own validation pass before wisp is recommended for production fleets. Phase 0.5.
- **`isd ps` backend column.** Operator-side dashboard / `isd` doesn't surface "this host runs backend=wisp" yet. Tracked for 0.5 polish.

## Demo status

The end-to-end install + controller bring-up + operator-side `isd deploy` was scoped down to the WispBackend smoke test for 0.4: the agent process talks to wisp directly, the smoke test exercises the full RuntimeBackend surface against busybox on a real cgroup v2 host, and the install flow renders the right compose recipe with the right capabilities and mounts.

What's verified:
- WispBackend full lifecycle (pull, create, start, inspect, remove) against busybox on the wisp VM as root: green.
- BollardBackend regression on the same hello-world spec via the trait: green.
- `install/install.sh` with `ISENGARD_RUNTIME=wisp` renders `compose.wisp.yaml` (privileged + cgroup + /var/lib/wisp + no docker.sock + ISENGARD_RUNTIME=wisp env) on both Mac and the wisp VM.
- 202 lib tests + clippy (`-D warnings`) + rustfmt all green on `wisp/phase-0-4`.

What's NOT yet verified at the agent-process-with-real-controller layer:
- Full `install.sh` -> `controller-up` -> `agent enroll` -> `isd deploy <stack>` -> `wisp ls` shows the stack on the agent. The smoke test covers the runtime surface; the full operator loop adds the controller + enrollment + dashboard layers, none of which are wisp-specific. Tracked for the next milestone alongside operator-side `isd ps backend=wisp` polish.
- Networked end-to-end (compose stack with port publish, `curl <vm-ip>:80` reaching pingora reaching the wisp container). The smoke test runs without networking; the network attacher needs operator-pre-created bridges, scoped to 0.5.

The release notes call this honestly: 0.4 ships the runtime swap, with the operator-facing demo polished against real fleets in 0.5.

## Spec + plan

- Spec: [`docs/superpowers/specs/2026-05-10-wisp-phase-0-4-agent-integration-design.md`](superpowers/specs/2026-05-10-wisp-phase-0-4-agent-integration-design.md)
- Plan: [`docs/superpowers/plans/2026-05-10-wisp-phase-0-4-agent-integration.md`](superpowers/plans/2026-05-10-wisp-phase-0-4-agent-integration.md)
- 0.1 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_1.md`](RELEASE_NOTES_WISP_PHASE_0_1.md)
- 0.2 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_2.md`](RELEASE_NOTES_WISP_PHASE_0_2.md)
- 0.3 release notes: [`docs/RELEASE_NOTES_WISP_PHASE_0_3.md`](RELEASE_NOTES_WISP_PHASE_0_3.md)
