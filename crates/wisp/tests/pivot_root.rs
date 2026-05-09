//! pivot_root integration test.
//!
//! Reads `/proc/<pid>/mountinfo` from the host and asserts that the
//! container's root mount points at a different filesystem from the
//! host's `/`. The host-side mountinfo for an unprivileged container
//! is visible because the host has CAP_SYS_PTRACE and reads through
//! the procfs.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use tempfile::TempDir;
use wisp::ContainerState;

#[test]
fn mountinfo_root_is_new_root() {
    if common::requires_root("mountinfo_root_is_new_root") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    let bundle =
        common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "sleep 5"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);
    runtime.create("pivot", &bundle).expect("create");
    runtime.start("pivot").expect("start");

    let pid = runtime
        .state("pivot")
        .expect("state")
        .pid
        .expect("pid is recorded");

    // Host's mountinfo for itself: the entry for `/` shows the
    // device numbers of the host root.
    let host_mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").expect("read host /proc/self/mountinfo");
    let host_root_dev =
        mountinfo_dev_for_root(&host_mountinfo).expect("host has a root mount entry");

    // Container's mountinfo seen from the host. The container is in
    // its own mount namespace so its `/` should differ from the
    // host's.
    let child_mountinfo = std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
        .expect("read child /proc/<pid>/mountinfo");
    let child_root_dev =
        mountinfo_dev_for_root(&child_mountinfo).expect("child has a root mount entry");

    assert_ne!(
        host_root_dev,
        child_root_dev,
        "container's root device {child_root_dev:?} should differ from host's {host_root_dev:?}; \
         host_mountinfo head=\n{}",
        first_n_lines(&host_mountinfo, 3)
    );

    // Cleanup.
    let _ = runtime.kill("pivot", nix::sys::signal::Signal::SIGKILL);
    let _ = common::wait_until_state(
        &runtime,
        "pivot",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );
    runtime.delete("pivot", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}

/// Find the mountinfo line whose mount point is `/` and return its
/// `<major:minor>` field.
///
/// Each line is space-separated:
///
/// ```text
/// mount-id parent-id major:minor root mount-point options - fstype source super-options
/// ```
///
/// We want the field at index 4 (mount-point) == "/" and return the
/// field at index 2 (major:minor).
fn mountinfo_dev_for_root(mountinfo: &str) -> Option<String> {
    for line in mountinfo.lines() {
        let mut fields = line.split_whitespace();
        let _mount_id = fields.next()?;
        let _parent_id = fields.next()?;
        let dev = fields.next()?;
        let _root = fields.next()?;
        let mount_point = fields.next()?;
        if mount_point == "/" {
            return Some(dev.to_string());
        }
    }
    None
}

fn first_n_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}
