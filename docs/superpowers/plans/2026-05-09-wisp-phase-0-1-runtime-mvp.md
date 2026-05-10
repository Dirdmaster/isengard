# Wisp Phase 0.1 Runtime MVP: Implementation Plan

> Spec: [`2026-05-09-wisp-phase-0-1-runtime-mvp-design.md`](../specs/2026-05-09-wisp-phase-0-1-runtime-mvp-design.md). Branch: `wisp/phase-0-1`.

## Scope

Land `crates/wisp/` (library) + `crates/wisp-cli/` (binary) with enough of an OCI runtime to run a hand-prepared busybox bundle: namespaces (PID + mount + UTS + IPC + network), mount setup, `pivot_root`, cgroup v2 limits, capability drop, exec the entrypoint as PID 1. CLI subcommands `run`, `ps`, `state`, `kill`, `delete`.

Out of scope: image pulling, networking primitives beyond a netns, overlay storage, rootless mode, seccomp/AppArmor/SELinux, hooks, cgroup v1, exec into running container (stretch only). Per the spec.

## Dev environment

OrbStack VM `wisp` (Ubuntu 24.04.4, kernel 6.19, cgroup v2, arm64). Already provisioned with rustup stable 1.95 + build-essential + pkg-config + libssl-dev. Mac home is bind-mounted at `/Users/dirdmaster/...` so the worktree edits land inside the VM with zero sync.

Loop: edit on Mac in `/Users/dirdmaster/Projects/isengard/.worktrees/next/`, then `orb -m wisp bash -lc "cd $WISP && cargo test -p wisp"`. Builds + runs natively on Linux; no rsync, no cross-compile.

The demo bundle uses an arm64 busybox binary. amd64 retargeting deferred per spec.

## Files touched

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | add `crates/wisp` + `crates/wisp-cli` to `members`; pin shared deps |
| `crates/wisp/Cargo.toml` | new, deps: `nix`, `caps`, `cgroups-rs`, `oci-spec`, `serde`, `serde_json`, `thiserror`, `tracing`, `bytes` |
| `crates/wisp/src/lib.rs` | new, public API surface |
| `crates/wisp/src/error.rs` | new, `WispError` enum |
| `crates/wisp/src/spec.rs` | new, `oci-spec` re-exports + validation helpers |
| `crates/wisp/src/state.rs` | new, state-dir layout + `ContainerHandle` persistence |
| `crates/wisp/src/cgroup.rs` | new, cgroup v2 controller setup |
| `crates/wisp/src/capability.rs` | new, bounding/ambient set logic |
| `crates/wisp/src/rlimit.rs` | new, `setrlimit` from spec |
| `crates/wisp/src/hostname.rs` | new, `sethostname` from spec |
| `crates/wisp/src/mount.rs` | new, rootfs + standard mounts + masked/readonly paths |
| `crates/wisp/src/pivot.rs` | new, `pivot_root` sequence |
| `crates/wisp/src/lifecycle/mod.rs` | new, create / start / kill / delete |
| `crates/wisp/src/lifecycle/clone.rs` | new, raw `clone3` wrapper, namespace flags |
| `crates/wisp/src/lifecycle/pipe.rs` | new, parent <-> child sync pipe |
| `crates/wisp/examples/run-busybox.rs` | new, end-to-end demo |
| `crates/wisp/examples/prepare-busybox.sh` | new, fetches busybox, writes config.json |
| `crates/wisp/tests/runtime_lifecycle.rs` | new, integration: create -> start -> wait -> delete |
| `crates/wisp/tests/mount_isolation.rs` | new, host /etc/passwd hidden inside |
| `crates/wisp/tests/cgroup_limits.rs` | new, memory.max enforces OOM |
| `crates/wisp/tests/cap_drop.rs` | new, dropped CAP_NET_BIND_SERVICE blocks port :80 |
| `crates/wisp-cli/Cargo.toml` | new, deps: `wisp`, `clap`, `anyhow`, `tracing-subscriber` |
| `crates/wisp-cli/src/main.rs` | new, subcommands: run / ps / state / kill / delete |
| `crates/wisp/README.md` | new, quickstart + VM dev loop |

## Steps

Each step ends with a commit. Branch is `wisp/phase-0-1`. Subagent dispatch model: Opus implementer per task, Sonnet code-reviewer at task boundaries that touch syscall surface.

### 1. Workspace skeleton

Add `crates/wisp` + `crates/wisp-cli` to workspace `members`. Both crates compile as no-op shells (`pub fn version() -> &'static str`, `fn main() { println!("wisp-cli stub"); }`). No deps beyond what's needed to compile. `cargo build -p wisp -p wisp-cli` from the VM passes.

Tests for this step: `cargo test -p wisp` runs (zero tests, exit 0). Confirms toolchain + workspace integration before any real work.

Commit: `feat(wisp): workspace skeleton (empty crates)`

### 2. WispError + spec parsing

`error.rs`: `WispError` enum with `Spec(oci_spec::ErrorImpl)`, `Io(std::io::Error)`, `Cgroup(String)`, `Mount(String)`, `Capability(String)`, `Lifecycle(String)`, `State(String)`. `thiserror::Error` derive.

`spec.rs`: re-export `oci_spec::runtime::Spec`. Add `pub fn load_and_validate(bundle: &Path) -> Result<Spec, WispError>` that loads `bundle/config.json`, asserts process.args is non-empty, asserts rootfs path exists relative to bundle, asserts linux.namespaces contains the five required (PID, mount, UTS, IPC, network), rejects user / cgroup namespaces with a clear "deferred to wisp 0.2" error.

Unit tests:
- valid minimal config: passes
- missing process.args: fails with clear message
- rootfs path nonexistent: fails
- user namespace declared: fails with the deferred-to-0.2 message
- cgroup namespace declared: fails with the deferred-to-0.2 message

Commit: `feat(wisp): WispError + OCI spec load and validate`

### 3. State directory

`state.rs`: `ContainerHandle` struct (id, bundle, state, pid, created_at), `ContainerState` enum (Created / Running / Stopped). Disk layout per the spec:

```
<state_dir>/containers/<id>/
  state.json
  config.json
  bundle.path
  log
```

Atomic writes via `tempfile::NamedTempFile::persist`. Public functions:

- `pub fn write(state_dir: &Path, h: &ContainerHandle) -> Result<()>`
- `pub fn read(state_dir: &Path, id: &str) -> Result<ContainerHandle>`
- `pub fn list(state_dir: &Path) -> Result<Vec<ContainerHandle>>` (skips entries that fail to parse, logs warn)
- `pub fn remove(state_dir: &Path, id: &str) -> Result<()>`

Unit tests:
- round-trip a handle: write -> read returns equal
- list with zero / one / many entries
- list tolerates a half-written `state.json` (truncate to 5 bytes mid-write, ensure that entry is skipped without erroring the whole list)
- remove cleans the dir; second remove is idempotent

Commit: `feat(wisp): state-dir layout and ContainerHandle persistence`

### 4. Cgroup v2

`cgroup.rs` with a thin trait so unit tests can mock the filesystem:

```rust
pub trait CgroupFs {
    fn write(&self, path: &Path, content: &[u8]) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn mkdir(&self, path: &Path) -> io::Result<()>;
    fn rmdir(&self, path: &Path) -> io::Result<()>;
}
```

`HostCgroupFs` is the prod impl (just `std::fs`). Tests use a `tempdir`-backed mock.

Public functions on a `Cgroup` struct (root: `/sys/fs/cgroup/wisp/`):

- `Cgroup::ensure_root() -> Result<()>`: creates the wisp slice, enables controllers via `cgroup.subtree_control`. Errors loudly if `/sys/fs/cgroup/cgroup.controllers` is absent (cgroup v1 detected).
- `Cgroup::create(id) -> Result<()>`: mkdir `<root>/<id>/`.
- `Cgroup::apply_resources(id, resources)`: writes memory.max, memory.swap.max, cpu.max, cpu.weight, pids.max from spec (`oci_spec::runtime::LinuxResources`).
- `Cgroup::add_pid(id, pid)`: appends to `<root>/<id>/cgroup.procs`.
- `Cgroup::kill_all(id)`: writes "1" to `cgroup.kill` if available; falls back to SIGKILL loop over `cgroup.procs`.
- `Cgroup::remove(id)`: rmdir; tolerates ENOENT.

Unit tests (with mock fs):
- ensure_root creates the directory + writes subtree_control
- apply_resources writes the correct files for each spec field; missing fields skipped
- cpu.shares -> cpu.weight mapping (2 -> 1, 1024 -> 100, 262144 -> 10000) matches the systemd table
- mock cgroup.kill missing falls back to procs read

Commit: `feat(wisp): cgroup v2 setup with mockable fs`

### 5. Capabilities

`capability.rs` using the `caps` crate. Functions:

- `drop_bounding(allowed: &[Capability]) -> Result<()>`: iterate the full bounding set, drop any not in `allowed`.
- `set_permitted(perms: &[Capability]) -> Result<()>`
- `set_effective(eff: &[Capability]) -> Result<()>`
- `set_inheritable(inh: &[Capability]) -> Result<()>`
- `raise_ambient(amb: &[Capability]) -> Result<()>`: `prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap, 0, 0)` per cap.
- `from_oci(set: &Vec<String>) -> Vec<Capability>`: map OCI cap names ("CAP_NET_BIND_SERVICE") to `caps::Capability`.

Unit tests:
- `from_oci` accepts known names, rejects garbage with a helpful error
- mapping is exhaustive: every `Capability` enum variant has an OCI name (caught by a test that round-trips Display -> from_oci for all variants)

Real prctl calls live behind `#[cfg(target_os = "linux")]`. Mac builds compile (the unit tests above don't call prctl).

Commit: `feat(wisp): capability set application via caps crate`

### 6. Mount setup + pivot_root

`mount.rs` and `pivot.rs`. Linux-only modules; gate the syscall-touching code behind `#[cfg(target_os = "linux")]`. Mac builds skip the integration tests; the orchestration code (pure logic) still compiles.

`mount::setup_rootfs(bundle: &Path, spec_mounts: &[oci_spec::runtime::Mount]) -> Result<()>`:
1. `mount("none", "/", null, MS_REC | MS_SLAVE, null)`
2. Bind-mount bundle/rootfs onto itself with `MS_BIND | MS_REC`
3. For each spec mount, `nix::mount::mount` it under the bundle/rootfs prefix
4. Standard device bind-mounts (`/dev/null`, `zero`, `full`, `random`, `urandom`, `tty`)

`mount::apply_masked_paths(rootfs: &Path, paths: &[String])`: bind-mount `/dev/null` over each.

`mount::apply_readonly_paths(rootfs: &Path, paths: &[String])`: remount with `MS_RDONLY`.

`pivot::pivot_root(new_root: &Path) -> Result<()>`:
1. `mkdir(new_root.join(".old"))`
2. `nix::unistd::pivot_root(new_root, new_root.join(".old"))`
3. `chdir("/")`
4. `umount(".old", MNT_DETACH)`
5. `rmdir(".old")`

Unit tests (Mac OK):
- `setup_rootfs` builds the correct mount calls list (test against a recording fake)
- masked / readonly path lists round-trip in the right order

Integration tests (VM only, gated): see step 11.

Commit: `feat(wisp): rootfs mounts, pivot_root, and masked / readonly paths`

### 7. clone3 + sync pipe + lifecycle wiring

`lifecycle/clone.rs`: raw `clone3` syscall via `libc::syscall(SYS_clone3, ...)`. `nix`'s clone3 wrapper has unstable API in 0.30+, so we go raw to keep control.

```rust
pub struct CloneArgs {
    pub flags: u64,            // CLONE_NEWPID | NEWNS | NEWUTS | NEWIPC | NEWNET
    pub exit_signal: i32,      // SIGCHLD
}

pub unsafe fn clone3(args: &CloneArgs) -> Result<CloneResult, WispError> {
    // returns Parent { child_pid } or Child
}
```

`lifecycle/pipe.rs`: `pipe2(O_CLOEXEC)` returning `(read_fd, write_fd)`. Helpers `wait_ready(read_fd) -> Result<()>` and `signal_ready(write_fd) -> Result<()>` reading / writing the literal bytes "ready".

`lifecycle/mod.rs`: ties everything together. Two functions:

- `pub fn create_container(rt: &Runtime, id: &str, bundle: &Path) -> Result<ContainerHandle>`: implements step 1 in the spec's "create" sequence (parse + validate spec, allocate state-dir, set up cgroup, return handle in Created state).
- `pub fn start_container(rt: &Runtime, id: &str) -> Result<()>`: implements the spec's "start" sequence (open sync pipe; clone3; in child: drop caps, setup_rootfs, pivot_root, chdir, hostname, rlimits, set ambient, signal_ready, exec; in parent: cgroup add_pid, wait_ready, write state.json with state=Running).

Unit tests are limited (the syscalls only run on Linux), but we can test the orchestration via a mocked sub-runner trait.

Integration tests live in step 11.

Commit: `feat(wisp): clone3 lifecycle + sync pipe + create/start wiring`

### 8. Runtime public API + kill / delete / state / list

`lib.rs`: `Runtime` struct that owns `state_dir` + `Cgroup`. Methods are thin wrappers over the lifecycle / state functions:

```rust
pub struct Runtime {
    state_dir: PathBuf,
    cgroup: Cgroup,
}

impl Runtime {
    pub fn new(state_dir: &Path) -> Result<Self, WispError> { /* mkdir state_dir, ensure cgroup root */ }
    pub fn create(&self, id: &str, bundle: &Path) -> Result<ContainerHandle, WispError> { lifecycle::create_container(self, id, bundle) }
    pub fn start(&self, id: &str) -> Result<(), WispError> { lifecycle::start_container(self, id) }
    pub fn kill(&self, id: &str, sig: nix::sys::signal::Signal) -> Result<(), WispError> {
        // read state, kill PID, leave state.json untouched (transition happens lazily on next state())
    }
    pub fn delete(&self, id: &str, force: bool) -> Result<(), WispError> {
        // refuse if Running and !force; else cgroup.kill_all + cgroup.remove + state::remove
    }
    pub fn state(&self, id: &str) -> Result<ContainerHandle, WispError> {
        // read state.json; if Running but /proc/<pid> gone, transition to Stopped + persist
    }
    pub fn list(&self) -> Result<Vec<ContainerHandle>, WispError> { state::list(&self.state_dir) }
}
```

Unit tests:
- `state()` transitions Running -> Stopped when pid is gone (test via a fake proc-existence checker injected at construction)
- `delete(force=false)` on a Running container errors
- `delete(force=true)` calls cgroup.kill_all then state::remove (verify via mocks)

Commit: `feat(wisp): Runtime public API`

### 9. wisp-cli binary

`crates/wisp-cli/src/main.rs` with `clap` derive. Subcommands:

- `wisp run <bundle> [--id <id>] [--detach]`: `Runtime::create` + `Runtime::start`. Without `--detach`, blocks until PID 1 exits (waits via `waitpid`), then `Runtime::delete --force`. With `--detach`, prints the container ID and returns.
- `wisp ps`: `Runtime::list`, prints id / state / pid / created.
- `wisp state <id>`: `Runtime::state` as JSON.
- `wisp kill <id> [--signal SIGTERM]`: signal parse via `nix::sys::signal::Signal::from_str`-equivalent.
- `wisp delete <id> [--force]`: `Runtime::delete`.

Default state-dir: `/var/lib/wisp` (overridable via `--state-dir` global flag or `WISP_STATE_DIR` env).

Tracing init: `tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter("warn").init()` (verbose with `WISP_LOG=debug`).

Unit tests: clap parses the flags. Behavior tests live in step 11 (VM integration).

Commit: `feat(wisp-cli): run / ps / state / kill / delete subcommands`

### 10. Demo bundle

`crates/wisp/examples/prepare-busybox.sh`:
- Detect arch (`uname -m` -> `aarch64` or `x86_64`)
- Download static busybox binary from `https://www.busybox.net/downloads/binaries/1.36.1-defconfig-multiarch-musl/busybox-{arch}` (or the equivalent stable URL)
- Build `examples/busybox/rootfs/bin/busybox` + symlinks for `/bin/sh`, `/bin/echo`, `/bin/hostname`
- Write `examples/busybox/config.json` with the OCI runtime spec (args=["/bin/sh","-c","echo hello && hostname"], hostname="wisp-demo", root.path="rootfs", linux.namespaces with the five we require, capabilities pruned to a minimal set, no readonly process)

Idempotent: re-running the script overwrites cleanly.

`crates/wisp/examples/run-busybox.rs`:
```rust
fn main() -> anyhow::Result<()> {
    let bundle = std::env::args().nth(1).unwrap_or("examples/busybox".into());
    let rt = wisp::Runtime::new(Path::new("/var/lib/wisp"))?;
    let h = rt.create("demo", Path::new(&bundle))?;
    rt.start(&h.id)?;
    // poll state until Stopped, then delete force
    loop {
        let s = rt.state(&h.id)?;
        if s.state == wisp::ContainerState::Stopped { break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    rt.delete(&h.id, true)?;
    Ok(())
}
```

Commit: `feat(wisp): busybox demo bundle and example`

### 11. Integration test suite

`crates/wisp/tests/runtime_lifecycle.rs` and friends. Each test builds a fixture bundle in a tempdir, runs the full Runtime API, asserts.

- `runtime_lifecycle.rs`: create -> state=Created. start -> state=Running, pid != None, `/proc/<pid>` exists, `/proc/<pid>/ns/pid` inode differs from host's. wait for exit. state -> Stopped. delete -> state-dir cleaned, cgroup removed.
- `mount_isolation.rs`: spec args runs `cat /etc/wisp-marker` (a file we write into the bundle's rootfs) and `! cat /etc/host-marker` (a file on the host but NOT in the bundle).
- `cgroup_limits.rs`: spec sets memory.max=64M; entrypoint allocates 128M via `dd if=/dev/zero of=/dev/null bs=1M count=128 conv=fsync`-style memory pressure. Expect OOM-kill (exit 137 or the cgroup's `memory.events` shows oom_kill > 0).
- `cap_drop.rs`: spec drops `CAP_NET_BIND_SERVICE`; entrypoint runs busybox `nc -l -p 80`; expect EACCES.
- `pivot_root.rs`: read `/proc/self/mountinfo` inside container, assert root mount is `/` not `/old-root`.

Tests gated on `cfg(target_os = "linux")` and run via `orb -m wisp bash -lc "cd $WISP && cargo test -p wisp --tests"`. Tests requiring root either:
- run inside the OrbStack VM as root by default (the `orb -m wisp -u root bash` form), or
- skip via a `requires_root!()` macro that early-returns if `geteuid() != 0`.

The CI question (Linux runner with cgroup v2 + KVM) is deferred: GitHub Actions stock runners are Ubuntu 22 with cgroup v1 unified mode broken; we'll cross that bridge when wisp 0.4 starts integrating with isengard-agent. For 0.1, "tests pass on the OrbStack VM" is the bar.

Commit: `test(wisp): integration tests for lifecycle / mount / cgroup / caps`

### 12. README + dev docs

`crates/wisp/README.md`:
- One-paragraph what + why
- "Run the demo" walkthrough (5 lines: orb shell, prepare bundle, cargo run --example)
- "Develop on a Mac" section pointing at the OrbStack VM setup
- Public API doc link to docs.rs (for the future)
- Status: "Phase 0.1, alpha. Not for production. See spec link."

`crates/wisp-cli/README.md`: a pointer to `crates/wisp/README.md` plus the subcommand cheat sheet.

Workspace `README.md`: add a one-line wisp section linking to the crate.

Commit: `docs(wisp): README + dev loop quickstart`

### 13. PR

Open a draft PR `wisp/phase-0-1` -> `next`. Title: `feat(wisp): Phase 0.1 daemonless OCI runtime MVP`. Body summarizes the spec, lists what's in / out, links the spec doc, and notes "demo: `cargo run --example run-busybox` from the OrbStack `wisp` VM prints `hello\nwisp-demo`". Hold for review before merging.

## Validation

Per-step `cargo test -p wisp` from the OrbStack VM. Final integration:

- `orb -m wisp bash -lc "cd $WISP && cargo build --workspace --release"` succeeds (no other crate breaks from the new members).
- `orb -m wisp bash -lc "cd $WISP && cargo test -p wisp"` passes (all unit + integration tests).
- `orb -m wisp bash -lc "cd $WISP && cargo run --release --example run-busybox"` prints `hello\nwisp-demo` and exits 0.
- `orb -m wisp bash -lc "cd $WISP && cargo run --release -p wisp-cli -- run examples/busybox --detach && cargo run -p wisp-cli -- ps"` shows the container, `wisp kill demo` + `wisp delete demo` cleans up.
- `cargo clippy --workspace --all-targets -- -D warnings` clean (run from the VM; clippy on Mac will skip the cfg-linux blocks).
- `cargo fmt --check` clean.
- `cargo deny check` clean (new deps added to deny.toml allowlist if any new licenses surface).

## Risks

- **clone3 raw syscall ergonomics.** `nix` 0.30+ has a clone3 wrapper but it changes shape with each release. We go raw via `libc::syscall(SYS_clone3, ...)`. Risk: `SYS_clone3` constant only in `libc` 0.2.150+; pin or vendor the constant if missing on the workspace's locked version. Spike this in step 7 before committing the API shape.
- **Systemd cgroup delegation.** OrbStack VMs may have systemd managing the unified cgroup hierarchy. If `mkdir /sys/fs/cgroup/wisp/` returns EPERM, document the workaround (`systemd-run --scope --user wisp run ...`) for 0.1 and plan a real fix in 0.5 (`systemd-run --scope` wrapper inside the runtime).
- **Capability sequencing bugs.** Easy to drop the wrong cap before raising ambient and end up with a container that can't even exec. Add explicit tests for the order, and a `tracing::debug!` line at each set-application so failures are diagnosable.
- **Pivot_root mount propagation.** Wrong propagation flags surface as "can still see host /home". The integration test in step 11 catches this; spec it explicitly so the implementer doesn't shortcut.
- **Test root requirements.** Most tests need real root for namespace + cgroup syscalls. The OrbStack VM defaults to a non-root user; tests run via `orb -m wisp -u root bash -lc "cargo test ..."`. CI design (when we get there) needs the same pattern.

## Subagent dispatch

Per `feedback_implementer_opus`, implementers run on Opus. Reviewers (post-step) on Sonnet for cost. Sequence:

1. Steps 1-3 are pure Rust + workspace plumbing. One Opus implementer can do all three in a single dispatch with three commits.
2. Step 4 (cgroup) gets its own implementer: non-trivial mock-fs design.
3. Steps 5-6 (caps + mounts + pivot) are one implementer dispatch: they're closely related and share the syscall reading.
4. Step 7 (clone3 + lifecycle wiring) is the highest-risk step; dispatch alone with explicit instructions to spike the syscall first and stop if it doesn't compile. Hold for review before continuing.
5. Steps 8-10 (Runtime API + CLI + demo) one dispatch.
6. Step 11 (integration tests) one dispatch, runs on the VM.
7. Step 12 (docs) one dispatch.
8. Step 13 (PR) handled in main session.

Code review at: end of step 4, end of step 7, end of step 11, before step 13.

## Open questions during implementation

- **`wisp init` re-exec.** runc has a tiny `runc init` that gets re-exec'd inside the cloned child to do post-clone setup. We attempt the inline-closure path first (clone3 with a child-fn closure). If `nix`/raw clone3 ergonomics force us to re-exec, we add a `wisp init <state-fd>` subcommand to wisp-cli and document it as an internal-only entry point. Decision in step 7.
- **`oci-spec` version.** Pin to whatever's current (>=0.7) and update `deny.toml` if needed. The crate has had API churn historically; confirm the rev compiles before the rest of step 2.
- **Bundle path persistence on move.** Spec says "error out if bundle moved between create and start." If real-world dev fights this, we copy the bundle into state-dir at create time. Defer the decision until the demo works.
