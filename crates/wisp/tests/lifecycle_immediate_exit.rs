//! Phase 0.17 Bug B regression: an immediate-exit container ships a
//! clean `Stopped` + `exit_status = 0` state, with no nsenter or
//! attach-to-ns errors in the lifecycle log.
//!
//! `#[ignore]`, root-only, Linux-only. Drives:
//!   1. `Runtime::create` on a busybox bundle whose entrypoint is
//!      `/bin/sh -c 'exit 0'`. Pre-0.17 a short-lived workload like
//!      this could race the parent's nsenter-based veth attach when
//!      a network spec was present.
//!   2. `Runtime::start` (no network: the unit-only path exercises
//!      the GoPipe wiring; the wisp_compose_e2e test covers the
//!      with-network shape).
//!   3. Polls the state until it reaches `Stopped` (with a generous
//!      timeout: the immediate-exit path goes through the reaper).
//!   4. Asserts `exit_status` is 0.
//!
//! Run inside the wisp VM as root:
//!
//!   cargo test -p wisp --test lifecycle_immediate_exit \
//!     -- --ignored --nocapture
//!
//! Bug B's repro shape (short-lived workload + network attach) is
//! covered by the agent-side integration in
//! `crates/isengard-agent/tests/wisp_nginx_cap_add_e2e.rs`: that test
//! drives the full WispBackend stack including the bridge + veth
//! plumbing. Here we just want to prove the GoPipe -> wait_go ->
//! exec -> exit sequence is clean even without a NetworkAttacher.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use tempfile::TempDir;
use wisp::ContainerState;

#[test]
#[ignore = "needs root + linux + cgroup v2"]
fn immediate_exit_busybox_reaches_stopped_with_exit_0() {
    if common::requires_root("immediate_exit_busybox_reaches_stopped_with_exit_0") {
        return;
    }

    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    // The repro: an entrypoint that exec's, runs, and exits all in
    // one syscall pair. Pre-0.17, with a network spec set, this
    // raced the parent's nsenter calls.
    let bundle = common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "exit 0"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    let id = "imexit";
    let handle = runtime.create(id, &bundle).expect("create");
    assert_eq!(handle.state, ContainerState::Created);

    runtime.start(id).expect("start");

    // Two transitions race: the reaper's 500ms tick writes
    // exit_status THEN flips state to Stopped, but Runtime::state
    // also lazily transitions Running -> Stopped when /proc/<pid>
    // is gone (without writing exit_status). Poll until both have
    // settled by checking handle.exit_code is Some, not just state.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut last_handle = runtime.state(id).expect("runtime.state");
    while std::time::Instant::now() < deadline {
        let h = runtime.state(id).expect("runtime.state");
        if h.state == ContainerState::Stopped && h.exit_code.is_some() {
            last_handle = h;
            break;
        }
        last_handle = h;
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        last_handle.state,
        ContainerState::Stopped,
        "container never reached Stopped (final handle: {last_handle:?})",
    );
    assert_eq!(
        last_handle.exit_code,
        Some(0),
        "exit_code was {:?}, expected Some(0)",
        last_handle.exit_code,
    );

    // Cleanup.
    runtime.delete(id, true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}
