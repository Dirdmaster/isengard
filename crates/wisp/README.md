# wisp

## What

Wisp is a daemonless Rust-native OCI runtime. Phase 0.1 is the runc-equivalent: takes an OCI bundle (a `config.json` + a `rootfs/` directory) and runs it in isolated namespaces (PID, mount, UTS, IPC, network) with cgroup v2 limits applied and a dropped capability set. No daemon, no socket, no broker process. The library + CLI live in this workspace and have zero `isengard-*` dependencies.

## Why

Isengard's agent currently shells out to `dockerd` via bollard. Every fleet feature we want next (replicated services, cross-host networking, controller-driven exec gates) has to map onto docker's primitives, and the agent ships a privileged `/var/run/docker.sock` mount inside its own container. Owning the runtime gives the orchestrator the lifecycle semantics it needs, removes the socket-as-attack-surface problem, and lets future layers stop routing around docker's swarm features.

Out of scope today (and tracked elsewhere): image pulling, networking primitives, overlay storage, rootless mode, seccomp / AppArmor / SELinux. See [`docs/superpowers/specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md`](../../docs/superpowers/specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md) for the full design and non-goals.

## Status

Phase 0.1, alpha. The busybox demo runs. Not for production. Linux-only (cgroup v2 unified hierarchy, kernel 5.3+). Root-only (the create path needs `CAP_SYS_ADMIN` for namespaces, mounts, and cgroup writes).

## Run the demo

The documented dev path: an OrbStack VM named `wisp` running a recent Debian or Ubuntu image, with the repo bind-mounted from the Mac.

```sh
# From the Mac, drop into the VM as root:
orb -m wisp -u root bash

# Inside the VM, build and run the bundled demo:
cd /Users/dirdmaster/Projects/isengard/.worktrees/next/crates/wisp
bash examples/prepare-busybox.sh
PATH=/home/dirdmaster/.cargo/bin:$PATH cargo run --example run-busybox
```

Expected output:

```text
hello
wisp-demo
```

`prepare-busybox.sh` lays down `examples/busybox/` (a static `busybox` in `rootfs/bin/` plus a minimal `config.json`). The example calls `Runtime::create` then `Runtime::start`, waits for PID 1 to exit, and tears the container down. Re-runs are idempotent.

## Develop on a Mac

The runtime needs Linux syscalls (`clone3`, `pivot_root`, namespace flags, cgroup v2). Day-to-day editing happens on the Mac; the test + run loop happens inside the OrbStack VM.

One-time setup:

```sh
# On the Mac:
orb create ubuntu wisp                  # or any cgroup-v2 distro
orb -m wisp bash -c 'curl https://sh.rustup.rs -sSf | sh -s -- -y'
```

OrbStack auto-mounts the host filesystem at the same path inside the VM, so your worktree is reachable at the same absolute path from both sides.

The loop:

1. Edit on the Mac (any editor that watches the host filesystem).
2. `orb -m wisp -u root bash` to drop into the VM as root.
3. `cargo build -p wisp` / `cargo test -p wisp` / `cargo run --example run-busybox` from inside the VM.

Mac-side `cargo build -p wisp -p wisp-cli` and `cargo test -p wisp` still work: the syscall code is `cfg(target_os = "linux")`-gated, and the macOS branches return `WispError::Lifecycle("wisp runtime requires linux")` from `create` / `start` / `kill`. The 54 unit tests pass on Mac (they cover spec parsing, state-dir layout, cgroup fs mocks, capability enum mapping, etc.).

Integration tests in `crates/wisp/tests/` print `SKIP <test>: requires root (run via \`orb -m wisp -u root\`)` and return early when not run as root in the VM. Inside the VM as root they exercise the real `clone3` / mount / cgroup path against tempdir state.

## Public API

```rust
use wisp::Runtime;

let rt = Runtime::new(state_dir)?;
let handle = rt.create(id, bundle)?;
rt.start(id)?;
let h = rt.state(id)?;
let all = rt.list()?;
rt.kill(id, nix::sys::signal::Signal::SIGTERM)?;
rt.delete(id, /* force */ true)?;
```

| Method | Description |
|--------|-------------|
| `Runtime::new(state_dir)` | Build a runtime against a state directory; creates it and the cgroup slice on Linux. |
| `Runtime::create(id, bundle)` | Parse + validate the bundle, persist a `Created` handle, prime the cgroup. No process is forked. |
| `Runtime::start(id)` | `clone3` PID 1 into fresh namespaces, apply mounts + `pivot_root` + caps, exec the entrypoint. |
| `Runtime::state(id)` | Read the on-disk handle; lazily transitions `Running -> Stopped` if `/proc/<pid>` is gone. |
| `Runtime::list()` | Walk the state directory and return every persisted handle. |
| `Runtime::kill(id, sig)` | Deliver a signal to PID 1; errors unless the container is `Running`. |
| `Runtime::delete(id, force)` | Remove the cgroup + state-dir entry. Refuses a `Running` container without `force=true`. |

`Runtime::start` calls `clone3`. The caller process must be single-threaded at that point (no tokio runtime, no background tracing worker). `wisp-cli` honours this; library users have to as well.

## Crate layout

```text
crates/wisp/src/
  lib.rs                  crate root + re-exports + version()
  error.rs                WispError + Result alias
  spec.rs                 OCI config.json load + validation
  state.rs                state-dir layout + ContainerHandle (de)serialisation
  cgroup.rs               cgroup v2 slice / kill_all / remove (mockable via CgroupFs)
  capability.rs           OCI capability name -> caps::Capability + apply()
  mount.rs                spec-driven mount setup (proc, sysfs, tmpfs, devpts, mqueue, bind)
  pivot.rs                pivot_root + masked / readonly path application (linux only)
  lifecycle/
    mod.rs                create_container / start_container glue
    clone.rs              clone3 wrapper + child entrypoint
    pipe.rs               parent <-> child sync pipe (handshake + error propagation)
  runtime.rs              Runtime struct: the public API surface
```

`crates/wisp/tests/` holds integration tests (`runtime_lifecycle`, `mount_isolation`, `pivot_root`, `cap_drop`, `cgroup_limits`); `crates/wisp/examples/` holds `prepare-busybox.sh` and `run-busybox.rs`.

## Roadmap

This crate covers Phase 0.1 only: bundle execution. The rest is tracked at the project level, not in this crate:

- Phase 0.2: image pulling (`wisp-image`).
- Phase 0.3: networking (`wisp-net`).
- Phase 0.4: agent integration (the Isengard agent stops talking to dockerd and drives `wisp::Runtime` directly).

For the in-scope feature list and non-goals, see [`docs/superpowers/specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md`](../../docs/superpowers/specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md).

## License

MIT (matches the workspace).
