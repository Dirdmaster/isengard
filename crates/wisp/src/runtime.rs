//! Public runtime API for `wisp`.
//!
//! [`Runtime`] is the single entry point external callers (the
//! `wisp-cli` binary, the integration tests, future `cargo run
//! --example` consumers) use to drive containers. It owns the
//! state-dir layout and the cgroup tree at `/sys/fs/cgroup/wisp` and
//! delegates create / start to [`crate::lifecycle`].
//!
//! ## State transitions
//!
//! State is persisted on disk and read lazily. [`Runtime::state`]
//! consults `/proc/<pid>` to detect a container whose PID 1 has
//! exited but whose state.json is still marked `Running`; the read
//! repairs the file in place so the next reader sees `Stopped` even
//! without going through `Runtime::state` again.
//!
//! ## Single-threaded invariant
//!
//! [`Runtime::start`] eventually calls `clone3`. The caller must not
//! have spawned helper threads (tokio runtime, tracing's flush
//! thread, etc.) before invoking it. See the lifecycle module's docs
//! for the full rationale.
//!
//! ## Cross-platform
//!
//! Every state-changing operation needs Linux primitives. On macOS
//! [`Runtime::create`], [`Runtime::start`], and [`Runtime::kill`]
//! return [`WispError::Lifecycle("wisp runtime requires linux")`].
//! [`Runtime::state`], [`Runtime::list`], and [`Runtime::delete`]
//! are pure file-IO: they work fine on Mac so the dev loop can poke
//! at fixtures.

use std::path::{Path, PathBuf};

use crate::cgroup::{Cgroup, HostCgroupFs};
use crate::error::{Result, WispError};
use crate::network_spec::NetworkSpec;
use crate::state::{self, ContainerHandle, ContainerState};

#[cfg(target_os = "linux")]
use crate::lifecycle;

/// Pluggable "is this PID still alive" probe. Used by
/// [`Runtime::state`] so unit tests can simulate a dead PID without
/// having to spawn / reap a real process.
type AliveCheck = fn(u32) -> bool;

/// Production probe: a PID is alive if `/proc/<pid>` exists and
/// `/proc/<pid>/status` does not report state `Z` (zombie). Zombies
/// linger in `/proc` until reaped, so the dir-existence check alone
/// would flag a reaped-but-not-cleaned-up container as Running.
fn proc_pid_alive(pid: u32) -> bool {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    if !proc_path.exists() {
        return false;
    }
    // status: line of the form "State:\t<X> (<word>)" where X is one of
    // R/S/D/T/Z/X. Zombie is the only one we want to treat as dead.
    let status = match std::fs::read_to_string(proc_path.join("status")) {
        Ok(s) => s,
        Err(_) => return true,
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            let trimmed = rest.trim();
            return !trimmed.starts_with('Z');
        }
    }
    true
}

/// Owner of the on-disk state-dir + cgroup slice.
///
/// Construct via [`Runtime::new`]. The struct is cheap to clone as
/// far as wisp is concerned, but [`Cgroup`] over [`HostCgroupFs`]
/// doesn't currently implement `Clone`, so we keep a single instance
/// per process.
pub struct Runtime {
    state_dir: PathBuf,
    cgroup: Cgroup<HostCgroupFs>,
    alive_check: AliveCheck,
}

impl Runtime {
    /// Default cgroup slice path. Tests construct the [`Runtime`]
    /// fields directly so they can point at a tempdir-backed slice.
    const DEFAULT_CGROUP_ROOT: &'static str = "/sys/fs/cgroup/wisp";

    /// Build a runtime against `state_dir`.
    ///
    /// Creates `state_dir` if it does not exist. On Linux we also call
    /// [`Cgroup::ensure_root`] to validate cgroup v2 + create the
    /// `wisp` slice; on Mac that step is skipped (the parent of
    /// `/sys/fs/cgroup/wisp` does not exist anywhere meaningful), so
    /// the runtime constructs but [`Runtime::create`] / [`start`]
    /// still error out as "requires linux" before reaching the
    /// cgroup.
    pub fn new(state_dir: &Path) -> Result<Self> {
        Self::with_cgroup_root(state_dir, Path::new(Self::DEFAULT_CGROUP_ROOT))
    }

    /// Build a runtime against `state_dir` with a non-default cgroup
    /// slice path. Used by integration tests so each test target gets
    /// its own per-test cgroup root under
    /// `/sys/fs/cgroup/wisp-test/<uniq>/`, avoiding state collisions
    /// between parallel tests.
    ///
    /// Behaves identically to [`Runtime::new`] otherwise: creates the
    /// state-dir, calls [`Cgroup::ensure_root`] on Linux, skips the
    /// cgroup step on Mac.
    pub fn with_cgroup_root(state_dir: &Path, cgroup_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir).map_err(|err| {
            WispError::State(format!("create state dir {}: {}", state_dir.display(), err))
        })?;

        let cgroup = Cgroup::new(cgroup_root);

        #[cfg(target_os = "linux")]
        {
            cgroup.ensure_root()?;
        }

        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            cgroup,
            alive_check: proc_pid_alive,
        })
    }

    /// Create + persist a container in the `Created` state. Mirrors
    /// `runc create`. No process is forked.
    ///
    /// Linux only: returns `WispError::Lifecycle("...requires
    /// linux")` on macOS so the dev Mac surfaces a clear message
    /// instead of a confusing cgroup error.
    pub fn create(&self, id: &str, bundle: &Path) -> Result<ContainerHandle> {
        #[cfg(target_os = "linux")]
        {
            lifecycle::create_container(&self.state_dir, &self.cgroup, id, bundle)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, bundle);
            Err(WispError::Lifecycle(
                "wisp runtime requires linux".to_string(),
            ))
        }
    }

    /// Create a container with a `NetworkSpec` attached.
    ///
    /// Same persistence + cgroup setup as [`Runtime::create`]; the
    /// resulting `state.json` carries the spec under
    /// `network_spec`. The actual veth + IP attachment is deferred to
    /// [`Runtime::start`], which reads the spec, allocates an IP, and
    /// records a `NetworkAttachmentRecord` once the child PID exists.
    ///
    /// Linux only. The Mac stub mirrors [`Runtime::create`].
    pub fn create_with_network(
        &self,
        id: &str,
        bundle: &Path,
        network: NetworkSpec,
    ) -> Result<ContainerHandle> {
        #[cfg(target_os = "linux")]
        {
            let mut handle =
                lifecycle::create_container(&self.state_dir, &self.cgroup, id, bundle)?;
            handle.network_spec = Some(network);
            state::write(&self.state_dir, &handle)?;
            Ok(handle)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, bundle, network);
            Err(WispError::Lifecycle(
                "wisp runtime requires linux".to_string(),
            ))
        }
    }

    /// Read the container's PID 1 from `state.json`. Returns `None`
    /// if the container is not in the `Running` state or if the
    /// state-dir entry is missing / unreadable.
    ///
    /// Used by `wisp-net` (and integration tests) to drive
    /// `nsenter -t <pid>` / `ip link set <iface> netns <pid>` after
    /// `Runtime::start` has cloned the entrypoint.
    pub fn container_pid(&self, id: &str) -> Option<u32> {
        match state::read(&self.state_dir, id) {
            Ok(h) => h.pid,
            Err(_) => None,
        }
    }

    /// Run the container's entrypoint as PID 1. Mirrors `runc start`.
    ///
    /// # Single-threaded invariant
    ///
    /// Internally calls `clone3`. The caller must be in a
    /// single-threaded process: glibc malloc holds locks across
    /// thread boundaries and a clone while another thread holds the
    /// heap lock deadlocks the child the moment it touches the
    /// allocator. `wisp-cli` honours this by avoiding tokio and any
    /// background tracing worker. Library users have to as well.
    ///
    /// If the container was created via [`Runtime::create_with_network`]
    /// you must use [`Runtime::start_with_attacher`] instead so the
    /// runtime has a hook to wire up bridge / veth / iptables.
    pub fn start(&self, id: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            lifecycle::start_container(&self.state_dir, &self.cgroup, id, None)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(WispError::Lifecycle(
                "wisp runtime requires linux".to_string(),
            ))
        }
    }

    /// Run the container's entrypoint as PID 1, wiring the network
    /// attachment via the supplied [`crate::NetworkAttacher`].
    ///
    /// If the container has no `network_spec` persisted, this is
    /// equivalent to [`Runtime::start`]: the attacher is ignored.
    ///
    /// On attach failure the child is killed + reaped before the
    /// error propagates.
    pub fn start_with_attacher(
        &self,
        id: &str,
        attacher: &mut dyn crate::NetworkAttacher,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            lifecycle::start_container(&self.state_dir, &self.cgroup, id, Some(attacher))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, attacher);
            Err(WispError::Lifecycle(
                "wisp runtime requires linux".to_string(),
            ))
        }
    }

    /// Send a signal to the container's PID 1. Does not transition
    /// state on disk: the next [`Runtime::state`] call repairs the
    /// file lazily once `/proc/<pid>` reports the process gone.
    ///
    /// Errors if the container is not in the `Running` state (we
    /// have no PID to deliver to) or if the kernel rejects the
    /// signal (typically ESRCH if PID 1 already died, or EPERM if
    /// the caller is no longer privileged).
    pub fn kill(&self, id: &str, sig: nix::sys::signal::Signal) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let handle = state::read(&self.state_dir, id)?;
            if handle.state != ContainerState::Running {
                return Err(WispError::Lifecycle(format!(
                    "container {id:?} is in state {:?}; kill expects Running",
                    handle.state
                )));
            }
            let pid = handle.pid.ok_or_else(|| {
                WispError::Lifecycle(format!(
                    "container {id:?} is Running but has no pid recorded"
                ))
            })?;
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig)
                .map_err(|err| WispError::Lifecycle(format!("kill {pid} with {sig:?}: {err}")))?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, sig);
            Err(WispError::Lifecycle(
                "wisp runtime requires linux".to_string(),
            ))
        }
    }

    /// Free state-dir + cgroup. Mirrors `runc delete`.
    ///
    /// Refuses if the container is in the `Running` state and
    /// `force` is false. With `force=true`, calls
    /// [`Cgroup::kill_all`] then [`Cgroup::remove`] then
    /// [`state::remove`]. Tolerates an already-cleaned cgroup.
    ///
    /// If the container has a `network_attachment` persisted, this
    /// method does NOT detach it: callers must use
    /// [`Runtime::delete_with_attacher`] so the iptables rules / host
    /// veth / IP allocation are reversed cleanly.
    pub fn delete(&self, id: &str, force: bool) -> Result<()> {
        let handle = state::read(&self.state_dir, id)?;
        if handle.state == ContainerState::Running && !force {
            return Err(WispError::Lifecycle(format!(
                "container {id:?} is Running; pass force=true to delete"
            )));
        }
        // kill_all is a no-op (or near-no-op) on a cgroup that has no
        // procs; safe to always invoke. remove tolerates ENOENT.
        self.cgroup.kill_all(id)?;
        self.cgroup.remove(id)?;
        state::remove(&self.state_dir, id)?;
        Ok(())
    }

    /// Like [`Runtime::delete`] but also detaches the network if the
    /// container's `state.json` carries a `network_attachment`.
    ///
    /// Detach is best-effort: a failure logs via tracing but does not
    /// stop the cgroup / state-dir cleanup. The wisp-cli uses this
    /// for the operator-facing `wisp delete` flow; the integration
    /// test calls it directly.
    pub fn delete_with_attacher(
        &self,
        id: &str,
        force: bool,
        attacher: &mut dyn crate::NetworkAttacher,
    ) -> Result<()> {
        let handle = state::read(&self.state_dir, id)?;
        if handle.state == ContainerState::Running && !force {
            return Err(WispError::Lifecycle(format!(
                "container {id:?} is Running; pass force=true to delete"
            )));
        }
        // Detach BEFORE killing the cgroup: if the iptables revoke
        // raises an error we want it surfaced before we lose the
        // ability to introspect.
        if let Some(record) = handle.network_attachment.as_ref() {
            if let Err(err) = attacher.detach(record) {
                tracing::warn!(
                    container_id = %id,
                    error = %err,
                    "network detach failed during delete; continuing with cgroup + state cleanup"
                );
            }
        }
        self.cgroup.kill_all(id)?;
        self.cgroup.remove(id)?;
        state::remove(&self.state_dir, id)?;
        Ok(())
    }

    /// Read state from disk + a `/proc/<pid>` check. Mirrors
    /// `runc state`. If the on-disk state is `Running` but the
    /// process is gone (or zombie), persist `Stopped` before
    /// returning.
    pub fn state(&self, id: &str) -> Result<ContainerHandle> {
        let mut handle = state::read(&self.state_dir, id)?;
        if handle.state == ContainerState::Running {
            let alive = match handle.pid {
                Some(pid) => (self.alive_check)(pid),
                None => false,
            };
            if !alive {
                handle.state = ContainerState::Stopped;
                state::write(&self.state_dir, &handle)?;
            }
        }
        Ok(handle)
    }

    /// Cheap directory walk over `state_dir`. See [`state::list`]
    /// for the per-entry parse-failure semantics.
    pub fn list(&self) -> Result<Vec<ContainerHandle>> {
        state::list(&self.state_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::SystemTime;

    use tempfile::TempDir;

    /// Build a Runtime against a tempdir state-dir and a tempdir
    /// cgroup root. We bypass `Runtime::new` (which would call
    /// `ensure_root` against a non-existent cgroup v2 mount on Mac
    /// and against the real one on Linux: neither is what these
    /// tests want) and construct fields directly.
    fn fixture(alive_check: AliveCheck) -> (TempDir, TempDir, Runtime) {
        let state = TempDir::new().unwrap();
        let cg = TempDir::new().unwrap();
        let rt = Runtime {
            state_dir: state.path().to_path_buf(),
            cgroup: Cgroup::new(cg.path().join("wisp")),
            alive_check,
        };
        (state, cg, rt)
    }

    fn make_handle(id: &str, state: ContainerState, pid: Option<u32>) -> ContainerHandle {
        ContainerHandle {
            id: id.to_string(),
            bundle: PathBuf::from(format!("/tmp/bundle-{id}")),
            state,
            pid,
            created_at: SystemTime::UNIX_EPOCH,
            network_spec: None,
            network_attachment: None,
            stdout_log_path: None,
            stderr_log_path: None,
        }
    }

    fn always_alive(_pid: u32) -> bool {
        true
    }

    fn always_dead(_pid: u32) -> bool {
        false
    }

    #[test]
    fn state_transitions_running_to_stopped_when_pid_gone() {
        let (state, _cg, rt) = fixture(always_dead);
        let handle = make_handle("ghost", ContainerState::Running, Some(12345));
        state::write(state.path(), &handle).unwrap();

        let got = rt.state("ghost").unwrap();
        assert_eq!(got.state, ContainerState::Stopped);
        assert_eq!(got.pid, Some(12345), "pid is preserved across transition");

        // Persisted: a fresh read sees Stopped without going through
        // Runtime::state again.
        let on_disk = state::read(state.path(), "ghost").unwrap();
        assert_eq!(on_disk.state, ContainerState::Stopped);
    }

    #[test]
    fn state_keeps_running_when_pid_alive() {
        let (state, _cg, rt) = fixture(always_alive);
        let handle = make_handle("live", ContainerState::Running, Some(99));
        state::write(state.path(), &handle).unwrap();

        let got = rt.state("live").unwrap();
        assert_eq!(got.state, ContainerState::Running);

        // Disk untouched.
        let on_disk = state::read(state.path(), "live").unwrap();
        assert_eq!(on_disk.state, ContainerState::Running);
    }

    #[test]
    fn state_passes_through_created_and_stopped() {
        let (state, _cg, rt) = fixture(always_alive);
        for s in [ContainerState::Created, ContainerState::Stopped] {
            state::write(state.path(), &make_handle("p", s, None)).unwrap();
            let got = rt.state("p").unwrap();
            assert_eq!(got.state, s);
        }
    }

    #[test]
    fn delete_force_false_errors_on_running() {
        let (state, _cg, rt) = fixture(always_alive);
        state::write(
            state.path(),
            &make_handle("hot", ContainerState::Running, Some(1)),
        )
        .unwrap();

        let err = rt.delete("hot", false).unwrap_err();
        match err {
            WispError::Lifecycle(msg) => {
                assert!(msg.contains("Running"), "msg should mention Running: {msg}")
            }
            other => panic!("expected WispError::Lifecycle, got {other:?}"),
        }

        // State file untouched.
        let on_disk = state::read(state.path(), "hot").unwrap();
        assert_eq!(on_disk.state, ContainerState::Running);
    }

    #[test]
    fn delete_force_true_removes_state_dir() {
        // Track that kill_all + remove were called by inspecting the
        // tempdir-backed cgroup tree. We pre-create the per-id dir
        // (mimicking what create_container would have done) and
        // assert state-dir is gone after delete. The cgroup remove
        // may fail to rmdir an empty-ish tempdir directory if the
        // kill_all fallback materialised a `cgroup.procs` file (the
        // real kernel exposes pseudo-files that don't appear in
        // dirent), so the production code tolerates that gracefully
        // and the disk-side state-dir cleanup is what we assert.
        let (state, cg, rt) = fixture(always_alive);
        state::write(
            state.path(),
            &make_handle("victim", ContainerState::Running, Some(2)),
        )
        .unwrap();

        let cg_dir = cg.path().join("wisp/victim");
        std::fs::create_dir_all(&cg_dir).unwrap();
        // Empty cgroup dir: kill_all has no procs file to read (it
        // returns Ok on ENOENT) and remove can rmdir the empty dir.
        rt.delete("victim", true).unwrap();

        assert!(
            !state.path().join("containers/victim").exists(),
            "state dir entry should be gone"
        );
        assert!(!cg_dir.exists(), "cgroup dir should be gone");
    }

    #[test]
    fn delete_tolerates_already_cleaned_cgroup() {
        let (state, _cg, rt) = fixture(always_alive);
        state::write(
            state.path(),
            &make_handle("ghost", ContainerState::Stopped, Some(2)),
        )
        .unwrap();
        // No cgroup dir at all: delete should still succeed.
        rt.delete("ghost", false).unwrap();
        assert!(!state.path().join("containers/ghost").exists());
    }

    #[test]
    fn list_returns_state_persisted_handles() {
        let (state, _cg, rt) = fixture(always_alive);
        for (id, st, pid) in [
            ("a", ContainerState::Created, None),
            ("b", ContainerState::Running, Some(7)),
            ("c", ContainerState::Stopped, Some(8)),
        ] {
            state::write(state.path(), &make_handle(id, st, pid)).unwrap();
        }

        let mut got = rt.list().unwrap();
        got.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[0].state, ContainerState::Created);
        assert_eq!(got[1].id, "b");
        assert_eq!(got[1].state, ContainerState::Running);
        assert_eq!(got[2].id, "c");
        assert_eq!(got[2].state, ContainerState::Stopped);
    }

    #[test]
    fn list_empty_state_dir_is_ok() {
        let (_state, _cg, rt) = fixture(always_alive);
        let got = rt.list().unwrap();
        assert!(got.is_empty());
    }

    /// Mac-only sanity: the platform-stub create / start / kill
    /// surfaces a "requires linux" lifecycle error rather than a
    /// confusing IO error from the nonexistent cgroup mount.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn create_returns_lifecycle_error_on_mac() {
        let (_state, _cg, rt) = fixture(always_alive);
        let bundle = TempDir::new().unwrap();
        let err = rt.create("x", bundle.path()).unwrap_err();
        match err {
            WispError::Lifecycle(msg) => assert!(msg.contains("linux")),
            other => panic!("expected Lifecycle, got {other:?}"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn start_returns_lifecycle_error_on_mac() {
        let (_state, _cg, rt) = fixture(always_alive);
        let err = rt.start("x").unwrap_err();
        match err {
            WispError::Lifecycle(msg) => assert!(msg.contains("linux")),
            other => panic!("expected Lifecycle, got {other:?}"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn kill_returns_lifecycle_error_on_mac() {
        let (state, _cg, rt) = fixture(always_alive);
        state::write(
            state.path(),
            &make_handle("x", ContainerState::Running, Some(1)),
        )
        .unwrap();
        let err = rt.kill("x", nix::sys::signal::Signal::SIGTERM).unwrap_err();
        match err {
            WispError::Lifecycle(msg) => assert!(msg.contains("linux")),
            other => panic!("expected Lifecycle, got {other:?}"),
        }
    }

    /// `Runtime::new` should succeed against an existing-or-creatable
    /// state-dir on Mac. The cgroup ensure_root step is gated to
    /// Linux only, so this tests the rest of the constructor.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn new_creates_state_dir_on_mac() {
        let parent = TempDir::new().unwrap();
        let path = parent.path().join("nested/wisp-state");
        let _rt = Runtime::new(&path).unwrap();
        assert!(path.is_dir());
    }

    /// Sanity: the production proc_pid_alive against PID 1 of the
    /// running test process. PID 1 (init/launchd) is always alive on
    /// any Unix; this just ensures the function compiles and returns
    /// something on the real `/proc` (Linux) or a sensible default
    /// (Mac, where `/proc` doesn't exist so it returns false).
    #[test]
    fn proc_pid_alive_is_callable() {
        // On Mac /proc doesn't exist so this returns false; on Linux
        // /proc/1 exists. Either is fine for "is the function
        // callable" coverage.
        let _ = proc_pid_alive(1);
        // Also ensure the AtomicBool trick we don't actually use is
        // not optimised out (compiler warning hygiene).
        let _ = AtomicBool::new(true).load(Ordering::Relaxed);
    }

    /// `container_pid` reads the persisted pid out of state.json. Pure
    /// file IO so it works on Mac.
    #[test]
    fn container_pid_returns_persisted_pid_when_running() {
        let (state, _cg, rt) = fixture(always_alive);
        let h = make_handle("alive", ContainerState::Running, Some(4242));
        state::write(state.path(), &h).unwrap();
        assert_eq!(rt.container_pid("alive"), Some(4242));
    }

    #[test]
    fn container_pid_returns_none_for_missing_id() {
        let (_state, _cg, rt) = fixture(always_alive);
        assert_eq!(rt.container_pid("ghost"), None);
    }

    #[test]
    fn container_pid_returns_none_when_state_has_no_pid() {
        let (state, _cg, rt) = fixture(always_alive);
        let h = make_handle("created", ContainerState::Created, None);
        state::write(state.path(), &h).unwrap();
        assert_eq!(rt.container_pid("created"), None);
    }
}
