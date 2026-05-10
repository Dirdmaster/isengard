# Wisp Phase 0.1: daemonless OCI runtime MVP

> Foundation for v0.4. Branch: `wisp/phase-0-1`. Not in `next` yet, no operator-facing change for v0.3.x users.

## What this is

`crates/wisp/` is a Rust-native OCI container runtime. It takes an OCI bundle (a directory with `config.json` and `rootfs/`) and runs the entrypoint inside isolated Linux namespaces with cgroup v2 limits, capabilities pruned per spec, and a `pivot_root`-ed filesystem view. No daemon, no socket, no docker dependency.

`crates/wisp-cli/` is the operator surface: `wisp run | ps | state | kill | delete`.

This is the v0.4 foundation: when wisp ships at 0.4 (agent integration), Isengard's `compose_reconciler` calls wisp instead of bollard, and the agent stops mounting `/var/run/docker.sock`.

## What's NOT in 0.1

The runtime is the thinnest possible runc-equivalent. Image pulling, networking primitives beyond a netns, overlay storage, rootless mode, seccomp / AppArmor / SELinux, hooks (prestart / poststart / poststop), cgroup v1 fallback, and exec-into-running-container are all explicitly deferred:

- 0.2 adds `wisp-image` (OCI distribution client + content store + layer extractor). Spec: `docs/superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md`.
- 0.3 adds `wisp-net` (bridge per agent, veth, IP allocation, port forwarding).
- 0.4 wires it into the agent: the actual replacement of dockerd.
- 0.5 polishes (cgroup-systemd integration, exec / logs / inspect, image GC).

## Done bar

`cargo run --example run-busybox` from inside an OrbStack Linux VM as root prints `hello\nwisp-demo` and exits 0. Verified end-to-end on `wisp` (Ubuntu 24.04.4, kernel 6.19, cgroup v2, arm64) in both debug + release.

Integration tests (Linux as root) cover lifecycle (create / start / stop / delete), mount isolation (host paths invisible inside, /proc mounted), cgroup v2 limits (memory.max, pids.max, cpu.weight via systemd-canonical mapping), capability drop (CAP_NET_BIND_SERVICE bit clear in `/proc/<pid>/status` CapBnd), and pivot_root (mountinfo root differs from host).

Test counts:
- Mac: 54 unit tests (cfg(linux) syscall code skip-compiles).
- Linux as root: 51 unit + 1 ignored clone3 smoke + 11 integration = 63 tests, all green, three-run non-flaky.

## Public API

```rust
use wisp::{Runtime, ContainerHandle, ContainerState, WispError};

let rt = Runtime::new(Path::new("/var/lib/wisp"))?;
let h = rt.create("demo", Path::new("/path/to/oci/bundle"))?;  // Created
rt.start(&h.id)?;                                              // Running, PID 1 in new namespaces
let s = rt.state(&h.id)?;                                      // lazy Running -> Stopped transition
rt.kill(&h.id, nix::sys::signal::Signal::SIGTERM)?;
rt.delete(&h.id, /* force */ false)?;                          // tear down state + cgroup
```

Test-only constructor `Runtime::with_cgroup_root(state_dir, cgroup_root)` lets integration tests run against an isolated `/sys/fs/cgroup/wisp-test/<unique>/` per test.

## Single-threaded invariant

`Runtime::start` calls `clone3` from the calling thread. Multi-threaded `clone3` deadlocks on glibc malloc. Callers MUST NOT spawn any threads (tokio runtime, std::thread::spawn) before invoking `start`. `wisp-cli` uses synchronous `tracing-subscriber::fmt` and plain `fn main` for this reason.

## Capability sequencing

The container's process gets caps applied in this order, derived from real-world failures during implementation:

1. After clone3, before pivot_root: `drop_bounding(spec.bounding)`.
2. Post-pivot, pre-exec: shrink effective set first, then permitted, then re-set effective to its target. The kernel rejects `set_permitted` if the new permitted is a strict subset of the current effective; the order above respects `new_effective ⊆ new_permitted` at every step.
3. Set inheritable.
4. Last: raise ambient one cap at a time via `prctl(PR_CAP_AMBIENT, RAISE, ...)`.

Skipping the effective-shrink pre-step yields EPERM at exec on real kernels; this regression is locked by integration tests.

## Cgroup v2 only

`Runtime::new` validates `/sys/fs/cgroup/cgroup.controllers` is present and lists `memory cpu pids`. v1 unified mode and the legacy hybrid hierarchy fail at boot with "wisp requires cgroup v2".

cpu.shares -> cpu.weight uses the systemd-canonical formula `1 + ((shares - 2) * 9999) / 262142`, NOT the simpler-looking `shares / 1024 * 100` mapping. Sample points: shares=2 -> weight=1; shares=1024 -> weight=39; shares=262144 -> weight=10000.

Memory swap math: OCI `memory.swap` is total `mem + swap`; cgroup v2 `memory.swap.max` is just swap. We compute `cgroup_swap = oci_swap - oci_memory` when both are set. Documented in `cgroup.rs`.

## Mounts

`mount::plan_mounts` is the testable, side-effect-free planner: takes a rootfs path + spec mounts, returns a `Vec<PendingMount>` describing what `setup_rootfs` will execute. Linux-only `setup_rootfs` walks the plan, calls `nix::mount::mount` per entry, then applies masked + readonly paths.

Notable footguns ironed out:
- Mount targets are auto-created (mkdir for dirs; touch for bind-from-device sources like `/dev/null` to avoid ENOENT).
- `tmpfs` over `/dev` mounts BEFORE the standard device bind-mounts (otherwise tmpfs wipes them).
- `apply_masked_paths` uses tmpfs for directory targets (e.g., `/sys/firmware`), not `/dev/null` bind (which fails ENOTDIR on directories).
- `apply_readonly_paths` bind-mounts paths onto themselves first, then remounts with `MS_RDONLY`. Procfs sub-paths can't be remounted directly; the bind-then-remount dance handles this.

## Known limitations

- Linux + cgroup v2 + root only. No rootless, no Mac runtime, no Windows.
- Single-host, no networking primitives. Container has a netns but no veth, no bridge, no DNS resolution beyond what's in the bundle's `/etc/resolv.conf`.
- No image cache. Bundles are hand-prepared (use `examples/prepare-busybox.sh` as a template).
- Cgroup-systemd interaction: distros that deny `Delegate=no` slices may EPERM at `/sys/fs/cgroup/wisp/` mkdir. OrbStack permits it; bare-metal Ubuntu typically does too. Distros that don't will need a `systemd-run --scope` wrapper landed in 0.5.
- Container PID 1 gets no built-in init. Forks-and-exits in the entrypoint produce zombies. Document for now; revisit in 0.4 if it bites real workloads.
- No image arch detection. The runtime accepts any bundle; Phase 0.1 demo uses an arm64 busybox because OrbStack runs arm64.

## Operator notes

This release ships as a separate set of crates inside the Isengard workspace; `next` users see no functional change. The branch `wisp/phase-0-1` is held back from merge until operator review of the structural pieces (12 commits, ~3500 lines of new code + tests).

To kick the tires:

```
orb create ubuntu:noble wisp     # one-time VM setup
orb -m wisp -u root bash -c "PATH=/home/dirdmaster/.cargo/bin:\$PATH; \
  cd /Users/dirdmaster/Projects/isengard/.worktrees/next/crates/wisp && \
  bash examples/prepare-busybox.sh && \
  cargo run --example run-busybox"
# expected: hello / wisp-demo
```

`crates/wisp/README.md` documents the dev loop in full.

## Spec + plan

- Spec: `docs/superpowers/specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md`
- Plan: `docs/superpowers/plans/2026-05-09-wisp-phase-0-1-runtime-mvp.md`
- 0.2 spec: `docs/superpowers/specs/2026-05-09-wisp-phase-0-2-image-pulling-design.md`
