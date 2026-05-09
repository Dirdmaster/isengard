//! Mount-isolation integration tests.
//!
//! Verifies that the container sees ITS OWN rootfs (not the host's
//! `/`) and that `/proc` was actually mounted in the container's
//! mount namespace.
//!
//! ## stdout / stderr capture pattern
//!
//! There's no built-in "give me the container's stdout" hook in
//! wisp 0.1; the entrypoint inherits the parent's fds. To capture
//! deterministically, the bundle bind-mounts a host tempdir to
//! `/wisp-out` inside the container and the args redirect stdout +
//! stderr to files there. After the container exits, the test reads
//! the host-side files.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use tempfile::TempDir;
use wisp::ContainerState;

#[test]
fn host_etc_passwd_invisible_inside() {
    if common::requires_root("host_etc_passwd_invisible_inside") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let out_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    // Plant a sentinel on the host that exists only outside the
    // container. The bundle's rootfs has /etc/ but it's empty (see
    // common::prepare_bundle) so a `cat` from inside MUST fail.
    let host_marker_path = "/etc/wisp-host-marker";
    let _ = std::fs::write(host_marker_path, b"host-only\n");
    let restore_marker = scopeguard_remove(host_marker_path);

    let bundle = common::prepare_bundle(
        bundle_tmp.path(),
        &[
            "/bin/sh",
            "-c",
            // Capture cat's exit code and the (empty) output. The
            // marker should be invisible inside the container.
            "cat /etc/wisp-host-marker > /wisp-out/stdout 2> /wisp-out/stderr; \
             echo EXIT=$? >> /wisp-out/stdout",
        ],
        &[],
        Some(out_tmp.path()),
    );
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);
    runtime.create("isol", &bundle).expect("create");
    runtime.start("isol").expect("start");
    let _ = common::wait_until_state(
        &runtime,
        "isol",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );

    let stdout = std::fs::read_to_string(out_tmp.path().join("stdout")).unwrap_or_default();
    let stderr = std::fs::read_to_string(out_tmp.path().join("stderr")).unwrap_or_default();
    assert!(
        stdout.contains("EXIT=1"),
        "cat should fail inside the container; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("No such file") || stderr.contains("not found"),
        "stderr should report missing file; got {stderr:?}"
    );

    runtime.delete("isol", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
    drop(restore_marker);
}

#[test]
fn procfs_is_mounted_inside() {
    if common::requires_root("procfs_is_mounted_inside") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let out_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    let bundle = common::prepare_bundle(
        bundle_tmp.path(),
        &[
            "/bin/sh",
            "-c",
            // /proc/self/status begins with Name: / Umask: / State:
            // / Tgid: / ... / Pid: <pid>. Capturing the first 6
            // lines is enough to assert /proc was mounted.
            "head -n 6 /proc/self/status > /wisp-out/stdout 2> /wisp-out/stderr",
        ],
        &[],
        Some(out_tmp.path()),
    );
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);
    runtime.create("proc", &bundle).expect("create");
    runtime.start("proc").expect("start");
    let _ = common::wait_until_state(
        &runtime,
        "proc",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );

    let stdout = std::fs::read_to_string(out_tmp.path().join("stdout")).unwrap_or_default();
    let stderr = std::fs::read_to_string(out_tmp.path().join("stderr")).unwrap_or_default();
    assert!(
        stdout.contains("Pid:"),
        "/proc must be mounted; stdout={stdout:?} stderr={stderr:?}"
    );

    runtime.delete("proc", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}

/// Drop guard that removes a host file on drop. We don't pull the
/// `scopeguard` crate just for this; a tiny RAII type is enough.
struct RemoveOnDrop(&'static str);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn scopeguard_remove(path: &'static str) -> RemoveOnDrop {
    RemoveOnDrop(path)
}
