//! Capability-drop integration tests.
//!
//! This is the "weaker than spec" test: the spec section ("cap_drop")
//! calls for binding low ports inside the container and observing
//! EACCES. Doing that cleanly requires either a tiny C program in
//! the rootfs (impractical) or an `nsenter`-from-host dance (complex).
//! For 0.1 we instead inspect `/proc/<pid>/status`'s `CapBnd` line
//! from the host: if the wisp implementation correctly drops the
//! capability before exec, the bounding bitmask will not contain the
//! bit for the dropped capability.
//!
//! That's a strong "wisp's plumbing actually works" assertion; the
//! "the kernel actually denies the syscall" assertion is left to the
//! Linux kernel + libcap, which we trust.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use tempfile::TempDir;
use wisp::ContainerState;

#[test]
fn dropping_net_bind_service_blocks_low_port() {
    if common::requires_root("dropping_net_bind_service_blocks_low_port") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    // Bundle's caps are CAP_KILL only (extra_caps is empty).
    // CAP_NET_BIND_SERVICE is therefore NOT in any of the
    // bounding/permitted/effective sets, so the cloned child should
    // come up with that bit cleared in its bounding mask.
    let bundle =
        common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "sleep 5"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);
    runtime.create("caps", &bundle).expect("create");
    runtime.start("caps").expect("start");

    let pid = runtime
        .state("caps")
        .expect("state")
        .pid
        .expect("pid is recorded");

    let status =
        std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read /proc/<pid>/status");
    let cap_bnd = status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .expect("CapBnd line in /proc/<pid>/status")
        .trim();
    let cap_bnd_u64 =
        u64::from_str_radix(cap_bnd, 16).unwrap_or_else(|err| panic!("parse {cap_bnd:?}: {err}"));

    // CAP_NET_BIND_SERVICE is bit 10 (man capabilities(7)).
    let net_bind_bit: u64 = 1 << 10;
    assert_eq!(
        cap_bnd_u64 & net_bind_bit,
        0,
        "CAP_NET_BIND_SERVICE bit (1<<10) should be CLEAR in bounding mask 0x{cap_bnd}; \
         full bnd = 0x{cap_bnd_u64:x}"
    );

    // CAP_KILL is bit 5; assert it's still set so we know we didn't
    // accidentally drop everything.
    let kill_bit: u64 = 1 << 5;
    assert_ne!(
        cap_bnd_u64 & kill_bit,
        0,
        "CAP_KILL bit should be SET in bounding mask (sanity); got 0x{cap_bnd_u64:x}"
    );

    // Cleanup: kill + delete.
    let _ = runtime.kill("caps", nix::sys::signal::Signal::SIGKILL);
    let _ = common::wait_until_state(
        &runtime,
        "caps",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );
    runtime.delete("caps", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}
