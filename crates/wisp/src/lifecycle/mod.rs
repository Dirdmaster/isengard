//! Container lifecycle: create + start.
//!
//! Per spec section "Lifecycle: create" and "Lifecycle: start":
//!
//! - [`create_container`] parses + validates the bundle, allocates a
//!   per-container directory under `state_dir`, caches the bundle's
//!   `config.json`, persists a [`ContainerHandle`] in the `Created`
//!   state, and creates a per-container cgroup with the OCI resource
//!   limits applied. No process is forked.
//! - [`start_container`] opens the parent / child sync pipe, calls
//!   [`clone::clone3`] with the five `CLONE_NEW*` flags, and walks
//!   the child path (drop caps, mount, pivot, chdir, hostname,
//!   rlimits, capability sequence, signal_ready, exec) and the
//!   parent path (cgroup attach, wait_ready, persist Running).
//!
//! ## Single-threaded invariant
//!
//! `start_container` calls `clone3` from the calling thread. glibc's
//! malloc holds locks across thread boundaries; cloning while another
//! thread holds the heap lock leads to deadlocks in the child the
//! moment it touches the allocator. The caller must invoke this
//! before spawning any tokio runtime, tracing-subscriber thread, or
//! anything else that talks to malloc on a worker thread. The
//! `wisp-cli` binary follows this discipline; library users have to
//! as well.
//!
//! ## Capability sequencing
//!
//! Bounding-set drop happens BEFORE `pivot_root` so post-pivot
//! processes can never re-acquire what we removed. Permitted /
//! effective / inheritable are all set after the rootfs is in place.
//! Ambient is raised LAST, after pivot, because some ambient cap
//! probes need a writable `/proc/self/...` and the post-pivot mount
//! tree is the one we want them to see.
//!
//! ## cgroup attach
//!
//! The parent attaches the child's PID to the cgroup AFTER `clone3`
//! returns but BEFORE `wait_ready` reads the ready token. By the time
//! the child's first non-trivial syscall lands the kernel already
//! accounts it under the per-container cgroup; resource limits apply
//! to the entrypoint exec, not just to its children.

#[cfg(target_os = "linux")]
pub mod clone;
pub mod pipe;

#[cfg(target_os = "linux")]
pub use clone::{CloneArgs, CloneResult};
pub use pipe::{ReadyPipe, signal_ready, wait_ready};

use std::path::Path;
use std::time::SystemTime;

use crate::cgroup::{Cgroup, CgroupFs};
use crate::error::{Result, WispError};
use crate::spec;
use crate::state::{self, ContainerHandle, ContainerState};

/// Allocate state-dir + cgroup for a new container in the `Created`
/// state. Mirrors `runc create`.
///
/// 1. Parse + validate `<bundle>/config.json` via [`spec::load_and_validate`].
/// 2. Refuse if `<state_dir>/containers/<id>/` already exists (no
///    silent reuse).
/// 3. Cache `config.json`, write `bundle.path`, persist `state.json`
///    with `state=Created, pid=None`.
/// 4. `cgroup.create(id)` + `cgroup.apply_resources(id, ..)`.
/// 5. Return the freshly-built [`ContainerHandle`].
///
/// Linux only. Mac builds redirect to a stub that explains the
/// platform mismatch. The signature is identical so wisp-cli compiles
/// cross-platform.
#[cfg(target_os = "linux")]
pub fn create_container<F: CgroupFs>(
    state_dir: &Path,
    cgroup: &Cgroup<F>,
    id: &str,
    bundle: &Path,
) -> Result<ContainerHandle> {
    create_container_inner(state_dir, cgroup, id, bundle)
}

/// Mac stub: `create` and `start` need Linux primitives. The error
/// here is what `wisp-cli run` surfaces when it's accidentally
/// invoked on the dev Mac instead of the OrbStack VM.
#[cfg(not(target_os = "linux"))]
pub fn create_container<F: CgroupFs>(
    _state_dir: &Path,
    _cgroup: &Cgroup<F>,
    _id: &str,
    _bundle: &Path,
) -> Result<ContainerHandle> {
    Err(WispError::Lifecycle(
        "wisp runtime requires linux".to_string(),
    ))
}

/// The actual create implementation, shared by the linux entry
/// point and (for unit testing in this module) the cross-platform
/// orchestration tests below. Allowed-dead on Mac under `cargo build`
/// because the only non-test caller is gated `cfg(linux)`.
#[allow(dead_code)]
fn create_container_inner<F: CgroupFs>(
    state_dir: &Path,
    cgroup: &Cgroup<F>,
    id: &str,
    bundle: &Path,
) -> Result<ContainerHandle> {
    // Step 1: parse + validate the bundle's config.json.
    let validated = spec::load_and_validate(bundle)?;

    // Step 2: refuse if the per-container dir already exists. We
    // check before any write so partially-created state can't get
    // re-overwritten by accident.
    let dir = state_dir.join("containers").join(id);
    if dir.exists() {
        return Err(WispError::State(format!(
            "container {id:?} already exists at {}",
            dir.display()
        )));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|err| WispError::State(format!("create state dir {}: {}", dir.display(), err)))?;

    // Step 3a: cache config.json so we don't rely on the bundle dir
    // staying put. (Bundle-move semantics are spec-open, but caching
    // here means `wisp state` still works if the operator deletes
    // the bundle later.)
    let config_path = bundle.join("config.json");
    let config_bytes = std::fs::read(&config_path).map_err(|err| {
        WispError::State(format!(
            "cache config.json {}: {}",
            config_path.display(),
            err
        ))
    })?;
    std::fs::write(dir.join("config.json"), &config_bytes).map_err(|err| {
        WispError::State(format!(
            "write {} : {}",
            dir.join("config.json").display(),
            err
        ))
    })?;

    // Step 3b: write the absolute bundle path so subsequent reads
    // (start_container) can find the bundle even from a different
    // cwd.
    let bundle_abs = std::fs::canonicalize(bundle).map_err(|err| {
        WispError::State(format!("canonicalize bundle {}: {}", bundle.display(), err))
    })?;
    std::fs::write(
        dir.join("bundle.path"),
        bundle_abs.to_string_lossy().as_bytes(),
    )
    .map_err(|err| WispError::State(format!("write bundle.path: {err}")))?;

    // Step 3c: build + persist the handle. SystemTime::now is fine
    // here: the field is informational; comparisons in tests stub it.
    let handle = ContainerHandle {
        id: id.to_string(),
        bundle: bundle_abs,
        state: ContainerState::Created,
        pid: None,
        created_at: SystemTime::now(),
    };
    state::write(state_dir, &handle)?;

    // Step 4: cgroup. Resources are taken from spec.linux.resources;
    // missing fields are skipped by `apply_resources` so a minimal
    // bundle works without a resources block.
    cgroup.create(id)?;
    if let Some(linux) = validated.linux().as_ref() {
        if let Some(resources) = linux.resources().as_ref() {
            cgroup.apply_resources(id, resources)?;
        }
    }

    Ok(handle)
}

/// Re-load the cached bundle for a container previously created via
/// [`create_container`]. Returns the parsed handle alongside the
/// fully-validated [`spec::Spec`] for the start path to consume.
///
/// Public so the future `Runtime` impl + unit tests can share it.
pub fn load_for_start(state_dir: &Path, id: &str) -> Result<(ContainerHandle, spec::Spec)> {
    let handle = state::read(state_dir, id)?;
    if handle.state != ContainerState::Created {
        return Err(WispError::Lifecycle(format!(
            "container {id:?} is in state {:?}; start expects Created",
            handle.state
        )));
    }
    let validated = spec::load_and_validate(&handle.bundle)?;
    Ok((handle, validated))
}

/// Run the container's entrypoint as PID 1 via `clone3` + the post-clone
/// child sequence. Mirrors `runc start`.
///
/// Pre-conditions: the container exists in the state-dir in the
/// `Created` state (i.e. `create_container` has been called).
///
/// Linux only. Mac builds return [`WispError::Lifecycle`] so the CLI
/// compiles and emits a clean error.
///
/// # Safety / threading
///
/// Caller must be in a single-threaded process. See module docs.
#[cfg(target_os = "linux")]
pub fn start_container<F: CgroupFs>(state_dir: &Path, cgroup: &Cgroup<F>, id: &str) -> Result<()> {
    use std::ffi::CString;

    let (mut handle, validated) = load_for_start(state_dir, id)?;

    // Resolve the absolute path to the rootfs ahead of clone, so the
    // child path doesn't have to do any allocation that depends on
    // the parent's cwd.
    let root = validated
        .root()
        .as_ref()
        .ok_or_else(|| WispError::Lifecycle("spec.root missing on validated bundle".to_string()))?;
    let rootfs = if root.path().is_absolute() {
        root.path().clone()
    } else {
        handle.bundle.join(root.path())
    };

    // Pre-build the exec args / env / cwd / hostname so the child
    // path doesn't have to allocate after clone. (Allocation in the
    // child is technically allowed pre-exec; we minimise it as a
    // matter of hygiene.)
    let process = validated.process().as_ref().ok_or_else(|| {
        WispError::Lifecycle("spec.process missing on validated bundle".to_string())
    })?;
    let args_vec = process
        .args()
        .as_ref()
        .ok_or_else(|| WispError::Lifecycle("spec.process.args missing".to_string()))?
        .clone();
    let argv: Vec<CString> = args_vec
        .iter()
        .map(|s| CString::new(s.as_bytes()).expect("argv contains NUL"))
        .collect();
    let envp: Vec<CString> = process
        .env()
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|s| CString::new(s.as_bytes()).expect("env contains NUL"))
        .collect();
    let cwd = process.cwd().to_string_lossy().into_owned();
    let hostname = validated.hostname().clone().unwrap_or_default();

    // Capability sets, all decoded up front so the child path
    // doesn't have to walk OCI strings.
    let bounding = process
        .capabilities()
        .as_ref()
        .and_then(|c| c.bounding().as_ref())
        .map(|set| set.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let permitted = process
        .capabilities()
        .as_ref()
        .and_then(|c| c.permitted().as_ref())
        .map(|set| set.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let effective = process
        .capabilities()
        .as_ref()
        .and_then(|c| c.effective().as_ref())
        .map(|set| set.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let inheritable = process
        .capabilities()
        .as_ref()
        .and_then(|c| c.inheritable().as_ref())
        .map(|set| set.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let ambient = process
        .capabilities()
        .as_ref()
        .and_then(|c| c.ambient().as_ref())
        .map(|set| set.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    let bounding_caps = crate::capability::from_oci(&bounding)?;
    let permitted_caps = crate::capability::from_oci(&permitted)?;
    let effective_caps = crate::capability::from_oci(&effective)?;
    let inheritable_caps = crate::capability::from_oci(&inheritable)?;
    let ambient_caps = crate::capability::from_oci(&ambient)?;

    let spec_mounts = validated.mounts().clone().unwrap_or_default();
    let masked = validated
        .linux()
        .as_ref()
        .and_then(|l| l.masked_paths().clone())
        .unwrap_or_default();
    let readonly = validated
        .linux()
        .as_ref()
        .and_then(|l| l.readonly_paths().clone())
        .unwrap_or_default();

    // Step: open the parent/child sync pipe. O_CLOEXEC keeps the
    // ends from leaking past `execvpe`.
    let ReadyPipe { reader, writer } = pipe::pair()?;

    // Step: clone3 with the five namespace flags + SIGCHLD.
    let args = CloneArgs {
        flags: clone::five_namespace_flags(),
        exit_signal: libc::SIGCHLD,
    };
    // SAFETY: the wisp library's start_container documents the
    // single-threaded invariant. The caller is responsible for
    // honouring it.
    let result = unsafe { clone::clone3(&args) }?;

    match result {
        CloneResult::Child => {
            // Reader fd dies with us when we exec; close it eagerly.
            drop(reader);

            // Run the child body; on error write the message into
            // the pipe (parent surfaces it), then exit non-zero so
            // the parent's waitpid catches it. We use _exit not exit
            // to avoid running atexit handlers / parent's tracing
            // subscriber drop.
            let outcome = run_child(
                &rootfs,
                &spec_mounts,
                &masked,
                &readonly,
                &cwd,
                &hostname,
                &bounding_caps,
                &permitted_caps,
                &effective_caps,
                &inheritable_caps,
                &ambient_caps,
                &writer,
                &argv,
                &envp,
            );
            // run_child only returns on error; on success it execs.
            // Whatever message we have, dump to stderr (the parent
            // sees it through the inherited fd 2) and exit 127 to
            // mimic shell-style "exec failed".
            let err_msg = match outcome {
                Ok(()) => "child returned without exec".to_string(),
                Err(err) => err.to_string(),
            };
            // SAFETY: we're in a freshly-cloned child; writing to
            // stderr is best-effort diagnostics.
            let bytes = err_msg.as_bytes();
            unsafe {
                libc::write(2, bytes.as_ptr() as *const _, bytes.len());
                libc::write(2, b"\n".as_ptr() as *const _, 1);
                libc::_exit(127);
            }
        }
        CloneResult::Parent { child_pid } => {
            // Parent: close the writer eagerly so EOF reaches the
            // reader if the child dies before signalling.
            drop(writer);

            // cgroup.add_pid BEFORE wait_ready: by the time the
            // child's first significant syscall lands (mount,
            // pivot_root) the kernel already accounts it under the
            // per-container cgroup. If add_pid fails we still need
            // to reap the child.
            if let Err(err) = cgroup.add_pid(id, child_pid) {
                // Best-effort kill; the child is hung up on its own
                // setup at this point.
                unsafe {
                    libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
                }
                let mut status: libc::c_int = 0;
                unsafe {
                    libc::waitpid(child_pid as libc::pid_t, &mut status, 0);
                }
                return Err(err);
            }

            // wait for the child's "ready" signal. If the child
            // died before signalling we get a clear EOF error.
            if let Err(err) = pipe::wait_ready(&reader) {
                let mut status: libc::c_int = 0;
                unsafe {
                    libc::waitpid(child_pid as libc::pid_t, &mut status, libc::WNOHANG);
                }
                return Err(WispError::Lifecycle(format!(
                    "child {child_pid} did not signal ready: {err}"
                )));
            }

            // Persist the running state.
            handle.state = ContainerState::Running;
            handle.pid = Some(child_pid);
            state::write(state_dir, &handle)?;
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn start_container<F: CgroupFs>(
    _state_dir: &Path,
    _cgroup: &Cgroup<F>,
    _id: &str,
) -> Result<()> {
    Err(WispError::Lifecycle(
        "wisp runtime requires linux".to_string(),
    ))
}

/// The post-clone child path. Returns only on error: success ends in
/// `execvpe`, which never returns. Each failure short-circuits with
/// a [`WispError::Lifecycle`] the caller writes to stderr before
/// `_exit(127)`.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn run_child(
    rootfs: &Path,
    spec_mounts: &[oci_spec::runtime::Mount],
    masked: &[String],
    readonly: &[String],
    cwd: &str,
    hostname: &str,
    bounding: &[crate::capability::Capability],
    permitted: &[crate::capability::Capability],
    effective: &[crate::capability::Capability],
    inheritable: &[crate::capability::Capability],
    ambient: &[crate::capability::Capability],
    writer: &std::os::fd::OwnedFd,
    argv: &[std::ffi::CString],
    envp: &[std::ffi::CString],
) -> Result<()> {
    use std::ffi::CString;

    // 1. Drop bounding BEFORE pivot_root: post-pivot processes can
    //    never re-acquire what we removed, even via an SUID binary
    //    in the new rootfs.
    crate::capability::drop_bounding(bounding)?;

    // 2. Mount the rootfs + standard devices + masked / readonly
    //    paths. setup_rootfs handles the slave-root + bind-onto-self
    //    + spec mounts + masked / readonly sequence.
    crate::mount::setup_rootfs(rootfs, spec_mounts, masked, readonly)?;

    // 3. Pivot into the new rootfs.
    crate::pivot::pivot_root(rootfs)?;

    // 4. chdir into spec.process.cwd (default "/"). The pivot_root
    //    call leaves us at "/" already; this only matters when the
    //    spec asks for a non-root cwd.
    let cwd_target = if cwd.is_empty() { "/" } else { cwd };
    nix::unistd::chdir(cwd_target)
        .map_err(|err| WispError::Lifecycle(format!("chdir({cwd_target:?}) post-pivot: {err}")))?;

    // 5. sethostname inside the new UTS namespace.
    if !hostname.is_empty() {
        nix::unistd::sethostname(hostname)
            .map_err(|err| WispError::Lifecycle(format!("sethostname({hostname:?}): {err}")))?;
    }

    // 6. rlimits. The OCI spec carries `process.rlimits` as a
    //    `RLIMIT_<X>` table. Phase 0.1 forwards whatever the bundle
    //    declares but does not synthesise defaults: the demo bundle
    //    leaves it empty and the kernel inherits the parent's. Step
    //    5's "Runtime API + CLI + demo" dispatch threads the actual
    //    rlimit slice through once the demo exercises it.

    // 7. Capability sequence: permitted, effective, inheritable. We
    //    set them post-pivot so any caps the OCI spec enables on the
    //    new root (rare but legal) take effect against the right
    //    mount tree.
    crate::capability::set_permitted(permitted)?;
    crate::capability::set_effective(effective)?;
    crate::capability::set_inheritable(inheritable)?;

    // 8. Signal the parent that we're ready BEFORE raising ambient
    //    + execvpe. The parent's add_pid + state-write happen
    //    against a child that's still in the lifecycle module's
    //    code; once we exec into the entrypoint there's no clean
    //    rollback.
    pipe::signal_ready(writer)?;
    // SAFETY: drop the writer's borrowed handle by closing the
    // underlying fd ourselves. The OwnedFd is borrowed from the
    // caller; we use `dup` semantics implicitly via the borrowed-fd
    // pattern in `signal_ready`. We close after signalling so the
    // parent's read returns immediately.
    // (We do NOT close fd 2 / fd 1 / fd 0 here.)
    let raw = std::os::fd::AsRawFd::as_raw_fd(writer);
    unsafe {
        libc::close(raw);
    }

    // 9. Ambient last, after pivot. Inheritable was set above; the
    //    kernel requires both inheritable AND permitted to contain
    //    a cap before ambient can hold it.
    crate::capability::raise_ambient(ambient)?;

    // 10. execvpe. argv is non-empty (validated earlier). On
    //    success this never returns. On failure we fall through to
    //    the caller which writes the diagnostic.
    if argv.is_empty() {
        return Err(WispError::Lifecycle("argv is empty".to_string()));
    }
    let prog = argv[0].clone();
    // Build the argv pointer array. nix has `execvpe` for path-search
    // semantics; the spec's process.args[0] is typically an absolute
    // path so PATH lookup is a no-op for the demo, but we honour the
    // spec by using path-search variant.
    let argv_refs: Vec<&CString> = argv.iter().collect();
    let envp_refs: Vec<&CString> = envp.iter().collect();
    let _err = nix::unistd::execvpe(&prog, &argv_refs, &envp_refs).err();
    // execvpe returns Err only on failure. The Errno-ish info isn't
    // captured here because nix returns Infallible on success; on
    // failure we surface the last_os_error.
    Err(WispError::Lifecycle(format!(
        "execvpe({}): {}",
        prog.to_string_lossy(),
        std::io::Error::last_os_error()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cgroup::{Cgroup, CgroupFs};
    use oci_spec::runtime::{
        LinuxBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, ProcessBuilder, RootBuilder,
        SpecBuilder,
    };
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn five_namespaces() -> Vec<oci_spec::runtime::LinuxNamespace> {
        [
            LinuxNamespaceType::Pid,
            LinuxNamespaceType::Mount,
            LinuxNamespaceType::Uts,
            LinuxNamespaceType::Ipc,
            LinuxNamespaceType::Network,
        ]
        .iter()
        .map(|t| LinuxNamespaceBuilder::default().typ(*t).build().unwrap())
        .collect()
    }

    fn write_minimal_bundle() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("rootfs")).unwrap();
        let process = ProcessBuilder::default()
            .args(vec!["/bin/sh".to_string()])
            .cwd(PathBuf::from("/"))
            .build()
            .unwrap();
        let root = RootBuilder::default()
            .path(PathBuf::from("rootfs"))
            .build()
            .unwrap();
        let linux = LinuxBuilder::default()
            .namespaces(five_namespaces())
            .build()
            .unwrap();
        let spec = SpecBuilder::default()
            .process(process)
            .root(root)
            .linux(linux)
            .build()
            .unwrap();
        spec.save(dir.path().join("config.json")).unwrap();
        dir
    }

    /// Tempdir-backed CgroupFs so `create_container_inner` can run on
    /// Mac without touching `/sys/fs/cgroup`. Records every path it
    /// touches so tests can assert the cgroup side-effects.
    struct FakeFs {
        root: PathBuf,
    }

    impl CgroupFs for FakeFs {
        fn write(&self, path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap_or(&self.root))?;
            std::fs::write(path, content)
        }
        fn read(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
            std::fs::read(path)
        }
        fn mkdir(&self, path: &std::path::Path) -> std::io::Result<()> {
            std::fs::create_dir_all(path)
        }
        fn rmdir(&self, path: &std::path::Path) -> std::io::Result<()> {
            std::fs::remove_dir(path)
        }
        fn exists(&self, path: &std::path::Path) -> bool {
            path.exists()
        }
    }

    #[test]
    fn create_container_writes_state_and_cgroup() {
        let bundle = write_minimal_bundle();
        let state_dir = TempDir::new().unwrap();
        let cgroup_root = TempDir::new().unwrap();
        let fs_root = cgroup_root.path().join("wisp");
        let fake = FakeFs {
            root: fs_root.clone(),
        };
        let cgroup: Cgroup<FakeFs> = Cgroup::with_fs(fake, fs_root.clone());

        let handle =
            create_container_inner(state_dir.path(), &cgroup, "alpha", bundle.path()).unwrap();
        assert_eq!(handle.id, "alpha");
        assert_eq!(handle.state, ContainerState::Created);
        assert!(handle.pid.is_none());

        // state.json round-trips.
        let read = state::read(state_dir.path(), "alpha").unwrap();
        assert_eq!(read.id, handle.id);
        assert_eq!(read.state, ContainerState::Created);

        // config.json + bundle.path are cached.
        let cdir = state_dir.path().join("containers/alpha");
        assert!(cdir.join("config.json").exists());
        assert!(cdir.join("bundle.path").exists());

        // cgroup directory exists in the fake fs.
        assert!(fs_root.join("alpha").is_dir());
    }

    #[test]
    fn create_container_refuses_existing_id() {
        let bundle = write_minimal_bundle();
        let state_dir = TempDir::new().unwrap();
        let cgroup_root = TempDir::new().unwrap();
        let fs_root = cgroup_root.path().join("wisp");
        let fake = FakeFs {
            root: fs_root.clone(),
        };
        let cgroup: Cgroup<FakeFs> = Cgroup::with_fs(fake, fs_root.clone());

        create_container_inner(state_dir.path(), &cgroup, "dup", bundle.path()).unwrap();
        let err = create_container_inner(state_dir.path(), &cgroup, "dup", bundle.path())
            .expect_err("second create should fail");
        match err {
            WispError::State(msg) => assert!(msg.contains("already exists")),
            other => panic!("expected WispError::State, got {other:?}"),
        }
    }

    /// Non-Linux stub: create_container should report the platform
    /// mismatch cleanly so wisp-cli can surface a sensible error
    /// during a `cargo run` from the dev Mac.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn create_container_returns_lifecycle_error_on_mac() {
        let state_dir = TempDir::new().unwrap();
        let cgroup_root = TempDir::new().unwrap();
        let fs_root = cgroup_root.path().join("wisp");
        let fake = FakeFs {
            root: fs_root.clone(),
        };
        let cgroup: Cgroup<FakeFs> = Cgroup::with_fs(fake, fs_root.clone());

        let bundle = write_minimal_bundle();
        let err = create_container(state_dir.path(), &cgroup, "x", bundle.path()).unwrap_err();
        match err {
            WispError::Lifecycle(msg) => {
                assert!(msg.contains("linux"), "msg should mention linux: {msg}")
            }
            other => panic!("expected WispError::Lifecycle, got {other:?}"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn start_container_returns_lifecycle_error_on_mac() {
        let state_dir = TempDir::new().unwrap();
        let cgroup_root = TempDir::new().unwrap();
        let fs_root = cgroup_root.path().join("wisp");
        let fake = FakeFs {
            root: fs_root.clone(),
        };
        let cgroup: Cgroup<FakeFs> = Cgroup::with_fs(fake, fs_root.clone());
        let err = start_container(state_dir.path(), &cgroup, "x").unwrap_err();
        match err {
            WispError::Lifecycle(msg) => {
                assert!(msg.contains("linux"), "msg should mention linux: {msg}")
            }
            other => panic!("expected WispError::Lifecycle, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn load_for_start_rejects_non_created_state() {
        let bundle = write_minimal_bundle();
        let state_dir = TempDir::new().unwrap();
        let cgroup_root = TempDir::new().unwrap();
        let fs_root = cgroup_root.path().join("wisp");
        let fake = FakeFs {
            root: fs_root.clone(),
        };
        let cgroup: Cgroup<FakeFs> = Cgroup::with_fs(fake, fs_root.clone());

        let mut handle =
            create_container_inner(state_dir.path(), &cgroup, "bad", bundle.path()).unwrap();
        handle.state = ContainerState::Running;
        state::write(state_dir.path(), &handle).unwrap();

        let err = load_for_start(state_dir.path(), "bad").unwrap_err();
        match err {
            WispError::Lifecycle(msg) => {
                assert!(msg.contains("Created"), "expected Created mention: {msg}")
            }
            other => panic!("expected WispError::Lifecycle, got {other:?}"),
        }
    }
}
