//! Shared helpers for wisp integration tests.
//!
//! Lives under `tests/common/mod.rs` (a sub-module path) rather than
//! `tests/common.rs` so cargo doesn't treat it as a separate test
//! binary. Each integration test file declares `mod common;` to pull
//! it in.
//!
//! ## Why a per-test cgroup root
//!
//! All tests share the host's `/sys/fs/cgroup` mount, so we can't use
//! the production wisp slice (`/sys/fs/cgroup/wisp/`) without
//! colliding with a real running runtime or with parallel test
//! invocations. Each test target gets a unique slice under
//! `/sys/fs/cgroup/wisp-test/<uniq>/`. The slice is removed at the
//! end of the test so re-running tests doesn't accumulate cruft.
//!
//! ## Why a per-test bundle
//!
//! `Runtime::create` rejects an existing state-dir entry, so even
//! parallel tests using the same id would race. Each test prepares
//! its own bundle in a `TempDir`; the shared piece is the busybox
//! binary, sourced from `examples/busybox/rootfs/bin/busybox` (or an
//! env-overridable path) so we don't re-download per test.

#![cfg(target_os = "linux")]
#![allow(dead_code)] // helpers used selectively by each test target

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wisp::{Cgroup, ContainerState, Runtime};

/// Returns true if the current process is uid 0.
///
/// Tests that need root call this at the top and early-return Ok(())
/// (printing a SKIP message) when it returns false. That way `cargo
/// test -p wisp --tests` on Mac or as a non-root Linux user produces
/// a flurry of "SKIP" lines and a green build.
pub fn is_root() -> bool {
    // `geteuid` is infallible per POSIX.
    nix::unistd::geteuid().is_root()
}

/// Per-test guard: prints a SKIP line and returns true if the test
/// should bail.
///
/// Idiom:
///
/// ```ignore
/// #[test]
/// fn foo() {
///     if requires_root("foo") { return; }
///     // ... test body ...
/// }
/// ```
pub fn requires_root(test_name: &str) -> bool {
    if !is_root() {
        eprintln!("SKIP {test_name}: requires root (run via `orb -m wisp -u root`)");
        return true;
    }
    false
}

/// Path to the busybox binary used by every test bundle.
///
/// Defaults to `crates/wisp/examples/busybox/rootfs/bin/busybox`
/// relative to the workspace root. Override with the
/// `WISP_TEST_BUSYBOX` env var if you want to point at a different
/// build.
pub fn busybox_path() -> PathBuf {
    if let Ok(p) = std::env::var("WISP_TEST_BUSYBOX") {
        return PathBuf::from(p);
    }
    // CARGO_MANIFEST_DIR is the wisp crate directory.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("examples/busybox/rootfs/bin/busybox")
}

/// Monotonic counter used to keep cgroup roots unique even within a
/// single process (per-test parallelism within a test target).
fn next_seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Build a unique cgroup root suitable for one test invocation.
///
/// Format: `/sys/fs/cgroup/wisp-test/<pid>-<unix-nanos>-<seq>/`. The
/// PID + nanos + seq combination keeps parallel tests (and parallel
/// `cargo test` invocations) from stomping on each other.
pub fn unique_cgroup_root() -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = next_seq();
    PathBuf::from(format!("/sys/fs/cgroup/wisp-test/{pid}-{nanos}-{seq}"))
}

/// Build a Runtime against an isolated state-dir + cgroup root.
///
/// `Cgroup::ensure_root` expects `<cgroup_root>/../cgroup.controllers`
/// to exist (it walks up to confirm we're on cgroup v2) AND writes
/// `<cgroup_root>/cgroup.subtree_control` to enable cpu/memory/pids
/// for any per-id child cgroups. For that write to succeed against a
/// freshly-mkdir'd cgroup, the *parent* has to already have those
/// controllers enabled in *its* subtree_control: cgroup v2 only
/// exposes a controller in a child's controllers when the parent's
/// subtree_control includes it.
///
/// We therefore:
///   1. Create the parent (`/sys/fs/cgroup/wisp-test/`).
///   2. Write `+cpu +memory +pids` to the parent's subtree_control
///      (idempotent: writes against an already-enabled controller
///      are no-ops).
///
/// After that, `ensure_root` against the per-test root proceeds
/// normally.
///
/// Panics on failure: if the runtime can't be constructed the test
/// can't run, so this is a clean way to surface the message in the
/// test output.
pub fn isolated_runtime(state_dir: &Path, cgroup_root: &Path) -> Runtime {
    if let Some(parent) = cgroup_root.parent() {
        // mkdir is enough on cgroup v2: the kernel synthesises
        // cgroup.controllers / cgroup.subtree_control / cgroup.procs
        // / etc. inside any directory under the v2 mount.
        let _ = fs::create_dir_all(parent);
        // Enable cpu/memory/pids in the parent's subtree_control so
        // ensure_root can in turn enable them in the per-test root's
        // subtree_control. The write may EBUSY if a previous test
        // already set it; that's fine, it's idempotent.
        let parent_subtree = parent.join("cgroup.subtree_control");
        if parent_subtree.exists() {
            let _ = fs::write(&parent_subtree, b"+cpu +memory +pids");
        }
    }
    Runtime::with_cgroup_root(state_dir, cgroup_root)
        .unwrap_or_else(|err| panic!("isolated_runtime ({}): {err}", cgroup_root.display()))
}

/// Best-effort cgroup teardown: kill any procs still in the slice and
/// rmdir it. Tolerant of an already-clean slice. Useful to call in a
/// test's cleanup epilogue or after a failing assertion.
pub fn teardown_cgroup_root(cgroup_root: &Path) {
    if !cgroup_root.exists() {
        return;
    }
    // The Cgroup type wraps the slice but doesn't expose a "remove
    // everything below me" call; we recursively rmdir each entry so
    // tests don't leak.
    let _ = remove_cgroup_recursive(cgroup_root);
    // The slice's parent (e.g. /sys/fs/cgroup/wisp-test/) is left
    // around; it's a single empty dir per test process and the kernel
    // cleans up on reboot.
}

fn remove_cgroup_recursive(path: &Path) -> std::io::Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    // Kill any processes in this cgroup before rmdir; otherwise
    // rmdir(2) returns EBUSY.
    let kill_path = path.join("cgroup.kill");
    if kill_path.exists() {
        let _ = fs::write(&kill_path, b"1");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            // Only descend into actual subdirs; pseudo-files in
            // cgroup v2 always have file metadata.
            let _ = remove_cgroup_recursive(&p);
        }
    }
    // rmdir is allowed on an empty cgroup directory (kernel-managed
    // pseudo-files don't count toward the dir's emptiness).
    let _ = fs::remove_dir(path);
    Ok(())
}

/// Bundle preparation: writes a config.json with the supplied args
/// and assembles a rootfs that has the busybox binary plus its
/// applet symlinks.
///
/// `extra_caps` adds capability names (e.g. `["CAP_NET_BIND_SERVICE"]`)
/// to bounding/permitted/effective; an empty slice yields a minimal
/// `["CAP_KILL"]` set. Note that an empty caps array entirely is
/// special-cased to drop everything; we always include CAP_KILL so
/// the entrypoint can still send signals during cleanup.
///
/// `out_dir`, when `Some`, is bind-mounted to `/wisp-out` in the
/// container so the test can capture stdout / stderr by having the
/// args redirect to `/wisp-out/stdout` and `/wisp-out/stderr`. Pass
/// `None` to skip the bind mount.
///
/// Returns the bundle directory (a child of `tmp`).
pub fn prepare_bundle(
    tmp: &Path,
    args: &[&str],
    extra_caps: &[&str],
    out_dir: Option<&Path>,
) -> PathBuf {
    let bundle = tmp.join("bundle");
    let rootfs = bundle.join("rootfs");
    for sub in [
        "bin",
        "etc",
        "proc",
        "sys",
        "dev",
        "dev/pts",
        "dev/shm",
        "dev/mqueue",
        "tmp",
        "wisp-out",
    ] {
        fs::create_dir_all(rootfs.join(sub)).expect("mkdir rootfs subdir");
    }

    // Place busybox + applet symlinks. We hard-link rather than copy
    // when possible (same filesystem) to keep test bundles lightweight;
    // fall back to copy on EXDEV.
    let bb_src = busybox_path();
    if !bb_src.is_file() {
        panic!(
            "busybox binary missing at {}; run `bash crates/wisp/examples/prepare-busybox.sh` \
             from the workspace root, or set WISP_TEST_BUSYBOX",
            bb_src.display()
        );
    }
    let bb_dst = rootfs.join("bin/busybox");
    match fs::hard_link(&bb_src, &bb_dst) {
        Ok(()) => {}
        Err(_) => {
            fs::copy(&bb_src, &bb_dst).expect("copy busybox");
        }
    }
    fs::set_permissions(&bb_dst, fs::Permissions::from_mode(0o755)).expect("chmod busybox");

    for applet in [
        "sh", "echo", "hostname", "cat", "ls", "true", "false", "sleep", "head", "wc", "yes", "nc",
        "dd", "jobs", "wait",
    ] {
        let link = rootfs.join("bin").join(applet);
        // Tolerate re-runs of the same target dir; ignore the AlreadyExists
        // case quietly.
        let _ = std::os::unix::fs::symlink("busybox", &link);
    }

    // Build config.json by string concat (avoid pulling oci-spec into
    // tests; the file format is small and stable).
    let caps_list: Vec<String> = std::iter::once("CAP_KILL".to_string())
        .chain(extra_caps.iter().map(|s| s.to_string()))
        .collect();
    let caps_json = caps_list
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let args_json = args
        .iter()
        .map(|a| {
            let escaped = a.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Optional bind mount for stdout/stderr capture. `bind` mounts
    // are spec-compliant and our `mount::setup_rootfs` understands
    // them.
    let extra_mount = match out_dir {
        Some(p) => format!(
            ",\n    {{\n      \"destination\": \"/wisp-out\",\n      \"type\": \"bind\",\n      \"source\": \"{}\",\n      \"options\": [\"bind\", \"rw\"]\n    }}",
            p.display()
        ),
        None => String::new(),
    };

    let config = format!(
        r#"{{
  "ociVersion": "1.0.2",
  "process": {{
    "terminal": false,
    "user": {{ "uid": 0, "gid": 0 }},
    "args": [{args_json}],
    "env": ["PATH=/bin:/usr/bin", "TERM=xterm"],
    "cwd": "/",
    "capabilities": {{
      "bounding":    [{caps_json}],
      "permitted":   [{caps_json}],
      "effective":   [{caps_json}],
      "inheritable": [],
      "ambient":     []
    }},
    "rlimits": [],
    "noNewPrivileges": true
  }},
  "root": {{
    "path": "rootfs",
    "readonly": false
  }},
  "hostname": "wisp-test",
  "mounts": [
    {{
      "destination": "/proc",
      "type": "proc",
      "source": "proc"
    }},
    {{
      "destination": "/dev",
      "type": "tmpfs",
      "source": "tmpfs",
      "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
    }},
    {{
      "destination": "/dev/pts",
      "type": "devpts",
      "source": "devpts",
      "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]
    }},
    {{
      "destination": "/dev/shm",
      "type": "tmpfs",
      "source": "shm",
      "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
    }},
    {{
      "destination": "/dev/mqueue",
      "type": "mqueue",
      "source": "mqueue",
      "options": ["nosuid", "noexec", "nodev"]
    }},
    {{
      "destination": "/sys",
      "type": "sysfs",
      "source": "sysfs",
      "options": ["nosuid", "noexec", "nodev", "ro"]
    }}{extra_mount}
  ],
  "linux": {{
    "namespaces": [
      {{ "type": "pid" }},
      {{ "type": "mount" }},
      {{ "type": "uts" }},
      {{ "type": "ipc" }},
      {{ "type": "network" }}
    ],
    "maskedPaths": [
      "/proc/kcore",
      "/proc/keys",
      "/proc/latency_stats",
      "/proc/timer_list",
      "/proc/timer_stats",
      "/proc/sched_debug",
      "/sys/firmware",
      "/proc/scsi"
    ],
    "readonlyPaths": [
      "/proc/asound",
      "/proc/bus",
      "/proc/fs",
      "/proc/irq",
      "/proc/sys",
      "/proc/sysrq-trigger"
    ]
  }}
}}"#
    );
    fs::write(bundle.join("config.json"), config).expect("write config.json");

    bundle
}

/// Build a bundle that already has a `linux.resources` block. Falls
/// back to `prepare_bundle` when no resources are needed.
///
/// `resources_json` is inserted as the value of the `linux.resources`
/// field; it must be a complete JSON object literal (e.g.
/// `r#"{ "memory": { "limit": 16777216 } }"#`).
pub fn prepare_bundle_with_resources(
    tmp: &Path,
    args: &[&str],
    extra_caps: &[&str],
    out_dir: Option<&Path>,
    resources_json: &str,
) -> PathBuf {
    let bundle = prepare_bundle(tmp, args, extra_caps, out_dir);
    let cfg_path = bundle.join("config.json");
    let original = fs::read_to_string(&cfg_path).expect("read config.json");
    // Splice in the resources field just before the closing `}` of
    // the linux block. We rely on the linux block being the last
    // top-level object value (it is in our template).
    let needle = "    ]\n  }\n}";
    let replacement = format!("    ],\n    \"resources\": {resources_json}\n  }}\n}}");
    let patched = original.replace(needle, &replacement);
    assert_ne!(
        patched, original,
        "linux.resources splice failed; template changed?"
    );
    fs::write(&cfg_path, patched).expect("rewrite config.json");
    bundle
}

/// Poll `runtime.state(id)` until it reaches `target` or `timeout`
/// expires. Returns the final state observed.
pub fn wait_until_state(
    runtime: &Runtime,
    id: &str,
    target: ContainerState,
    timeout: Duration,
) -> ContainerState {
    let start = Instant::now();
    loop {
        let handle = runtime.state(id).expect("runtime.state");
        if handle.state == target {
            return handle.state;
        }
        if start.elapsed() >= timeout {
            return handle.state;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read the inode of `/proc/self/ns/pid` for the host (the test
/// process). Used by lifecycle tests to verify that the container's
/// pid namespace differs from the host's.
pub fn host_pid_ns_inode() -> u64 {
    let meta = fs::metadata("/proc/self/ns/pid").expect("stat /proc/self/ns/pid");
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

/// Read the inode of `/proc/<pid>/ns/pid` for an arbitrary pid.
pub fn pid_ns_inode(pid: u32) -> std::io::Result<u64> {
    let meta = fs::metadata(format!("/proc/{pid}/ns/pid"))?;
    use std::os::unix::fs::MetadataExt;
    Ok(meta.ino())
}

/// Tear down a container that's been left in the state-dir / cgroup
/// after a test failure. Kills first, then deletes. Best-effort: any
/// failure is logged via stderr but doesn't propagate.
pub fn force_cleanup(runtime: &Runtime, id: &str) {
    let _ = runtime.delete(id, true);
}

/// Remove a per-test cgroup root (using the typed `Cgroup` API for
/// the per-id sub-tree under it). Best-effort.
pub fn cleanup_cgroup(cgroup_root: &Path, id: &str) {
    let cg = Cgroup::new(cgroup_root.to_path_buf());
    let _ = cg.kill_all(id);
    let _ = cg.remove(id);
    teardown_cgroup_root(cgroup_root);
}
