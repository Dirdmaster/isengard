# Wisp Phase 0.1: `wisp-runtime` MVP (design)

Status: proposed
Phase: v0.4 foundation, wisp 0.1
Author: 2026-05-09

## Problem

Isengard's agent talks to `dockerd` via `bollard`. Every fleet feature we
want next (replicated services, cross-host networking, rolling updates,
controller-driven exec gates, immutable workloads) has to map onto
docker's primitives, and we ship a daemon-mounted `/var/run/docker.sock`
inside the agent's container. That socket is a privilege escalation
hazard, the lifecycle semantics aren't ours to influence, and the
compose-as-truth flow already routes around docker's swarm features
because they're the wrong shape for our orchestrator.

Replacing the runtime is a months-long investment, but every layer above
gets cleaner once we own it. Phase 0.1 is the smallest deliverable that
proves we can: a Rust crate that takes an OCI runtime bundle and runs it.

## Goal

Land `crates/wisp/` with a `Runtime` API that creates, starts, stops, and
inspects an OCI container from a hand-prepared bundle directory. The CLI
companion `crates/wisp-cli/` (`wisp run <bundle>`) is the operator surface
for the demo. Pulling images, building bundles, networking, and storage
drivers are all out of scope and live in their own future crates.

The bar for v0.1 done: `wisp run examples/busybox/` execs `/bin/sh -c
'echo hello && hostname'` inside a fresh PID + mount + UTS + IPC + network
namespace, with cgroup v2 limits applied and the host rootfs hidden via
`pivot_root`. The host process tree shows the container PID under wisp;
`wisp ps` lists it; `wisp kill` tears it down cleanly.

## Non-goals (Phase 0.1)

- Image pulling. Bundles are hand-prepared (extract a tar, write
  `config.json`). `wisp-image` lands in 0.2.
- Networking. Container has a network namespace but no veth, no bridge,
  no DNS. `wisp-net` lands in 0.3.
- Overlay storage. Bundles' `rootfs/` is a plain directory the user
  prepared. `wisp-storage` lands in 0.5 territory.
- Rootless mode. User namespaces deferred to wisp 0.2.
- seccomp BPF, AppArmor, SELinux. Default-allow until 0.5+.
- cgroup v1. Hard floor: kernel 5.3+ with cgroup v2 unified hierarchy.
- Hooks (prestart / poststart / poststop).
- Devices beyond the standard set (`/dev/null`, `zero`, `full`, `random`,
  `urandom`, `tty`).
- CRI server, container runtime spec compliance certification.

## Design

### Crate layout

```
crates/
  wisp/                       # the runtime library
    Cargo.toml                # NO isengard-* deps
    src/
      lib.rs                  # public API (Runtime, ContainerHandle)
      error.rs                # WispError
      spec.rs                 # oci-spec re-exports + helpers
      state.rs                # state_dir on-disk layout, ContainerHandle
      lifecycle/
        mod.rs                # create / start / kill / delete
        clone.rs              # clone3 wrapper, namespace flags
        pipe.rs               # parent <-> child sync pipe
      mount.rs                # rootfs mounts, masked/readonly paths
      pivot.rs                # pivot_root sequence
      cgroup.rs               # cgroup v2 setup (memory/cpu/pids)
      capability.rs           # bounding/permitted/ambient set
      rlimit.rs               # rlimit setrlimit
      hostname.rs             # uts namespace + sethostname
    examples/
      run-busybox.rs          # `cargo run --example run-busybox`
    tests/
      runtime_lifecycle.rs    # integration: create -> start -> wait -> delete
  wisp-cli/                   # debugging + showcase binary
    Cargo.toml
    src/
      main.rs                 # `wisp run <bundle>`, `wisp ps`, `wisp kill`,
                              # `wisp state`, `wisp delete`
```

The `wisp` crate has zero `isengard-*` deps. Its dep set is
ecosystem-only: `nix`, `caps`, `cgroups-rs`, `oci-spec`, `serde`, `serde_json`,
`thiserror`, `tracing`, `bytes`. The `wisp-cli` crate adds `clap`,
`anyhow`, `tracing-subscriber`.

### Public API

```rust
// crates/wisp/src/lib.rs

pub struct Runtime {
    state_dir: PathBuf,        // e.g. /var/lib/wisp
    cgroup_root: PathBuf,      // e.g. /sys/fs/cgroup/wisp
}

impl Runtime {
    pub fn new(state_dir: &Path) -> Result<Self, WispError>;

    /// Parse `<bundle>/config.json`, allocate a state-dir entry,
    /// create the container in `Created` state. Does NOT start the
    /// process. Mirrors `runc create`.
    pub fn create(&self, id: &str, bundle: &Path) -> Result<ContainerHandle, WispError>;

    /// Run the entrypoint as PID 1 of the container. Mirrors `runc start`.
    pub fn start(&self, id: &str) -> Result<(), WispError>;

    /// Send a signal to the container's PID 1. Mirrors `runc kill`.
    pub fn kill(&self, id: &str, sig: nix::sys::signal::Signal) -> Result<(), WispError>;

    /// Free state-dir + cgroup. Errors if the container is still running
    /// unless `force=true`. Mirrors `runc delete`.
    pub fn delete(&self, id: &str, force: bool) -> Result<(), WispError>;

    /// Read state from disk + a /proc check. Mirrors `runc state`.
    pub fn state(&self, id: &str) -> Result<ContainerHandle, WispError>;

    /// Cheap directory walk over state_dir.
    pub fn list(&self) -> Result<Vec<ContainerHandle>, WispError>;

    // Phase 0.1 stretch (see "Stretch goals" below):
    // pub fn exec(&self, id: &str, cmd: &ExecSpec) -> Result<ExecHandle, WispError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHandle {
    pub id: String,
    pub bundle: PathBuf,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub created_at: SystemTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Stopped,
}
```

### State directory layout

```
/var/lib/wisp/
  containers/
    <id>/
      state.json        # serialized ContainerHandle
      config.json       # cached copy of the bundle's config.json
      bundle.path       # absolute path to bundle dir (so state.json stays portable)
      sync.pipe.r       # parent end of the parent<->child sync pipe
      log               # combined wisp-internal log for this container
```

State writes are atomic (write-then-rename). Reads tolerate partial writes
during create.

### Lifecycle: create

1. Parse `<bundle>/config.json` via `oci-spec`. Validate: rootfs path
   exists, process.args non-empty, linux.namespaces includes the five we
   require (PID, mount, UTS, IPC, network). Reject user/cgroup namespaces
   in 0.1: those are 0.2.
2. Allocate `<state_dir>/containers/<id>/`. Refuse if it exists.
3. Cache `config.json`, write `bundle.path`, write `state.json` with
   `state=Created, pid=None`.
4. Set up cgroup `/<cgroup_root>/<id>` and apply spec limits (memory, cpu,
   pids). Leave the cgroup empty: child will join it on clone.
5. Return `ContainerHandle`. The runtime process goes back to its caller
   without ever forking. (`runc` in fact forks at `create` and pauses; we
   defer the clone until `start` to keep the model simpler.)

### Lifecycle: start

This is where the work happens. Sequence:

```
parent (caller of Runtime::start)
  pipe(O_CLOEXEC) -> (read, write)         // parent <-> child sync
  clone3(CLONE_NEW{PID,NS,UTS,IPC,NET} | SIGCHLD)
    -> child PID

  child:                                    // PID 1 in new namespaces
    close(read)
    set_keep_caps(true)
    capability::drop_bounding_set(spec.capabilities.bounding)
    mount::setup_rootfs(spec.mounts, bundle/rootfs)
    pivot::pivot_root(bundle/rootfs)
    chdir(spec.process.cwd or "/")
    hostname::sethostname(spec.hostname)
    rlimit::apply(spec.process.rlimits)
    capability::set_ambient(spec.capabilities.ambient)
    write(write, "ready")                   // signal parent to record PID
    close(write)
    execvpe(spec.process.args[0], args, env)
    exit(127)                               // exec failed

  parent:
    close(write)
    cgroup::add_pid(child_pid)              // before any user-process work
    read(read, "ready")                     // child has pivot_rooted
    state.json: state=Running, pid=child_pid
    return Ok(())
```

Two design choices worth flagging:

- **cgroup attach happens in the parent.** The child writes to the cgroup
  via the parent's filesystem view of `/sys/fs/cgroup`. Doing it in the
  child after `pivot_root` would require either bind-mounting cgroupfs
  inside the rootfs (security smell) or holding an open fd to the cgroup
  directory before pivot. Parent-side attach is what runc does.
- **Capability sequencing.** Set `keep_caps(true)` before drop-bounding,
  so caps survive the implicit setuid that `pivot_root + clone3` does
  not perform but the OCI spec implies for any future user-namespace
  story. Drop bounding set BEFORE pivot_root so post-pivot processes
  can't acquire what we removed. Set ambient set last, after the rootfs
  is in place (some ambient caps need `/proc/self/...` writable).

### Mounts

Spec-driven, in order:

1. Make `/` slave (`mount("none", "/", null, MS_REC | MS_SLAVE, null)`)
   so our mount churn doesn't propagate back to the host's mount table.
2. Bind-mount the bundle's rootfs onto itself with `MS_BIND | MS_REC`.
   Required for pivot_root to accept it as a separate mount.
3. For each `mount` entry in spec.mounts, with `nix::mount`:
   - `proc` -> `/proc` inside rootfs
   - `tmpfs` -> `/dev` (size 65536k, mode 755)
   - `devpts` -> `/dev/pts` (newinstance, ptmxmode=0666)
   - `tmpfs` -> `/dev/shm`
   - `mqueue` -> `/dev/mqueue`
   - `sysfs` -> `/sys` (ro)
   - `cgroup` -> `/sys/fs/cgroup` (ro, type=cgroup2)
4. After pivot_root: bind-mount `/dev/null` over each path in
   `spec.linux.maskedPaths` (e.g. `/proc/kcore`, `/sys/firmware`).
5. Remount each path in `spec.linux.readonlyPaths` with `MS_RDONLY`.
6. Standard device nodes (`null`, `zero`, `full`, `random`, `urandom`,
   `tty`): bind-mount from host `/dev/<name>` into rootfs `/dev/<name>`.
   Creating mknod nodes inside the rootfs needs `CAP_MKNOD` which we may
   have dropped; bind-mounts side-step that.

### Cgroup v2

- Cgroup root: `/sys/fs/cgroup/wisp/`. Created on `Runtime::new` if
  missing. If `/sys/fs/cgroup/cgroup.controllers` is absent (cgroup v1
  unified is off), error out with a hard message: "wisp requires cgroup v2."
- For each container: `/sys/fs/cgroup/wisp/<id>/`. Enable controllers via
  `cgroup.subtree_control` on the parent.
- Apply spec.linux.resources:
  - `memory.max` from `memory.limit`
  - `memory.swap.max` from `memory.swap`
  - `cpu.max` from `cpu.quota` / `cpu.period`
  - `cpu.weight` from `cpu.shares` (with the standard 2-262144 mapping)
  - `pids.max` from `pids.limit`
- On delete: write the cgroup's `cgroup.procs` empty (kill any stragglers
  via `cgroup.kill` if available, fall back to SIGKILL loop), then
  `rmdir`.

### Capabilities

`caps` crate. The OCI spec provides five sets: `bounding`, `effective`,
`permitted`, `inheritable`, `ambient`. Ordering:

1. After clone3, before pivot_root: drop bounding to spec.bounding.
2. Set permitted = spec.permitted.
3. Set effective = spec.effective.
4. Set inheritable = spec.inheritable. Required for ambient.
5. After pivot_root: raise ambient = spec.ambient one cap at a time
   via `prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap, 0, 0)`.

If `spec.process.user.uid != 0`: `setresuid` after step 4 (before exec,
before ambient). 0.1 keeps the entrypoint as PID 1 root inside the
container; non-root user is supported but not exercised by the demo.

### CLI

`wisp-cli/src/main.rs`:

```
wisp run <bundle> [--id <id>] [--detach]
wisp ps
wisp state <id>
wisp kill <id> [--signal SIGTERM]
wisp delete <id> [--force]
```

`wisp run` is the convenience wrapper: `Runtime::create` then
`Runtime::start`, default `--detach=false` blocks until the container's
PID 1 exits, prints exit code, then `Runtime::delete --force`.
With `--detach`, `wisp run` returns immediately after start prints the
container ID. `wisp ps` lists running and stopped containers. State
auto-transitions Running -> Stopped when `Runtime::state` notices the PID
is gone.

### Demo bundle

`crates/wisp/examples/busybox/` (or generated by a script):

```
busybox/
  config.json            # OCI runtime spec, args=["/bin/sh","-c","echo hello && hostname"]
  rootfs/                # extracted busybox tarball
    bin/...
    lib/...
    ...
```

A small `examples/prepare-busybox.sh` downloads `busybox` from
`https://www.busybox.net/downloads/binaries/...`, extracts to `rootfs/`,
writes a minimal `config.json`. The script is part of the example; it
runs once per dev machine.

The `cargo run --example run-busybox` example does:

```rust
let rt = Runtime::new(Path::new("/var/lib/wisp"))?;
let h = rt.create("demo", Path::new("examples/busybox"))?;
rt.start(&h.id)?;
// wait for exit ... done in CLI normally, here just sleep + state-poll
rt.delete(&h.id, true)?;
```

## Test strategy

Unit tests (run on Mac, no Linux primitives):

- `spec`: parse + validate sample `config.json`s, accept good ones, reject
  bad ones (missing rootfs, no process.args, unsupported namespace).
- `cgroup`: a fake `/sys/fs/cgroup` directory tree (tempdir), exercise
  controller subtree-enable and resource-write logic via a `CgroupFs`
  trait we mock.
- `state`: round-trip `ContainerHandle` through `state.json`. List with
  zero / one / many entries. Tolerate partial writes (truncate mid-write,
  ensure list() skips it).

Integration tests (run inside the Linux dev VM):

- `runtime_lifecycle.rs`:
  - create from bundle -> state=Created
  - start -> state=Running, pid != None, /proc/<pid> exists, ns inode
    differs from host's
  - kill SIGTERM -> state=Stopped
  - delete -> state-dir cleaned, cgroup removed
- `mount_isolation.rs`: container can't see host's `/etc/passwd` (tested
  by reading inside via a small post-pivot probe added to a fixture
  bundle).
- `cgroup_limits.rs`: spec sets memory.max=64M; container allocates 128M;
  expect OOM-kill.
- `cap_drop.rs`: spec drops `CAP_NET_BIND_SERVICE`; container tries to
  bind :80; expect EACCES.
- `pivot_root.rs`: host's `/etc/hostname` is invisible inside; container's
  hostname matches spec.

CI for v0.1: skip the integration tests on macOS workers; run them on a
Linux runner. Local dev loop is "edit on Mac -> rsync -> cargo test on
the Lima VM" (Lima provides Ubuntu 24.04 with kernel 6.8+, cgroup v2
default-on, sufficient for everything in this spec).

## Stretch goals (only if Phase 0.1 lands fast)

- `Runtime::exec`: enter an existing container's namespaces via
  `setns(2)` + clone, run a one-shot command. Needed by `wisp exec` and,
  later, by `isd logs` integration. If we pull this in, the API gets:
  ```rust
  pub struct ExecSpec { args: Vec<String>, env: Vec<String>, cwd: PathBuf }
  pub struct ExecHandle { pid: u32, stdout: ChildStdout, stderr: ChildStderr }
  pub fn exec(&self, id: &str, cmd: &ExecSpec) -> Result<ExecHandle>;
  ```
- Container logs: have wisp-cli's foreground mode tee the child's stdout
  + stderr through pipes to its own stdout. Right now everything goes
  straight to the parent terminal (no redirect set up before exec).

## Risks

- **clone3 + glibc.** Older glibc wrap clone3 with `force_clone()` semantics
  that don't accept a `flags` arg the kernel expects. We skip glibc here
  and `nix::sched::clone3` directly via raw syscall (`libc::syscall(SYS_clone3, ...)`).
  Risk: `nix` 0.30+ has `clone3` but the API is unstable; we may have to
  wrap libc directly.
- **Cgroup-systemd interaction.** Most distros run systemd as the cgroup
  manager. Writing to `/sys/fs/cgroup/wisp/` under systemd's nose may
  fail with `EPERM` because systemd has set `Delegate=no` on its slices.
  Mitigation for 0.1: document "run wisp under a delegated slice or as
  PID 1's child." Real fix in 0.5: emit a `systemd-run --scope`
  invocation around the runtime.
- **PID-namespace zombie reaping.** Container's PID 1 is the entrypoint.
  When it exits, all descendants are reaped by the kernel because the
  pidns goes away. If a user-supplied entrypoint forks-and-exits (the
  classic shell-pipeline dangling-child bug), we get a zombie inside the
  container. 0.1 doesn't ship a built-in init; document the constraint
  ("use `--init` patterns or a real init like `tini`") and revisit if
  it bites the demo.
- **Pivot_root edge cases.** Need `MS_REC | MS_PRIVATE` propagation on
  the new root; the old root must be a different mount than the new
  root; rootfs must not be `/`. Adding e2e tests early to catch
  regressions.

## Out of scope, explicitly

- Replacing bollard in `isengard-agent`. That happens at v0.4 (wisp 0.4
  per the Overview phasing). 0.1 ships in `crates/wisp/` and is consumed
  only by `wisp-cli` and `cargo run --example`.
- Multi-arch. amd64 only for the demo. arm64 is a kernel/ABI rebuild
  away; defer until a real demand surfaces.
- Windows. Never.

## Open questions

- **Sync pipe vs. notify socket.** OCI runtime spec lets `runc start`
  signal the child via a notify socket the child opens. We're using a
  simple pipe between parent and child for "rootfs ready" sync. If we
  want full spec-compliance later (so other tooling can drive wisp), the
  notify socket layer slots in cleanly. Phase 0.1: pipe.
- **`wisp init` helper.** runc has a tiny `runc init` re-exec'd inside
  the child to do post-clone setup. We're doing it inline via a closure
  passed to `clone3`. `nix`'s clone3 wrapper accepts that pattern; if it
  fights us, we add a `wisp init` re-exec like runc does.
- **Bundle path persistence.** If the operator moves the bundle directory
  between create and start, we error out. Should we copy the bundle into
  state-dir at create time? Adds disk-write cost; the operator-error
  case is rare. Phase 0.1: error out, document.
