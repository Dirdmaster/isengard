//! Lifecycle integration tests: create / start / state / kill / delete.
//!
//! Linux only. Each test guards itself with `requires_root`: on Mac
//! or as a non-root user the body short-circuits with a SKIP line so
//! `cargo test -p wisp --tests` stays green everywhere.
//!
//! Per-test isolation:
//!   - state-dir: `tempfile::TempDir`
//!   - cgroup root: `/sys/fs/cgroup/wisp-test/<uniq>/`
//!   - bundle: `tempfile::TempDir` with a freshly-assembled rootfs

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use tempfile::TempDir;
use wisp::ContainerState;

#[test]
fn create_and_start_then_stop_then_delete_clean() {
    if common::requires_root("create_and_start_then_stop_then_delete_clean") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    // /bin/sh -c 'echo started; sleep 0.1' exits within ~100ms.
    let bundle = common::prepare_bundle(
        bundle_tmp.path(),
        &["/bin/sh", "-c", "echo started; sleep 0.1"],
        &[],
        None,
    );
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    let handle = runtime.create("life", &bundle).expect("create");
    assert_eq!(handle.state, ContainerState::Created);
    assert!(handle.pid.is_none());

    runtime.start("life").expect("start");
    let started = runtime.state("life").expect("state after start");
    assert_eq!(started.state, ContainerState::Running);
    let pid = started.pid.expect("pid after start");

    // Container PID namespace must differ from the host's.
    let host_ns = common::host_pid_ns_inode();
    let child_ns = common::pid_ns_inode(pid).expect("read child pid ns");
    assert_ne!(
        host_ns, child_ns,
        "container pid namespace ({child_ns}) must differ from host's ({host_ns})"
    );

    // Wait for the child to exit (sleep 0.1 -> Stopped within a few seconds).
    let final_state = common::wait_until_state(
        &runtime,
        "life",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );
    assert_eq!(
        final_state,
        ContainerState::Stopped,
        "container should reach Stopped within 5s"
    );

    runtime.delete("life", false).expect("delete");

    let cdir = state_tmp.path().join("containers/life");
    assert!(!cdir.exists(), "state-dir entry should be gone");
    let cgdir = cgroup_root.join("life");
    assert!(!cgdir.exists(), "cgroup dir should be gone");

    common::teardown_cgroup_root(&cgroup_root);
}

#[test]
fn start_writes_pid_to_state() {
    if common::requires_root("start_writes_pid_to_state") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    let bundle =
        common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "sleep 1"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    runtime.create("pidwrite", &bundle).expect("create");
    runtime.start("pidwrite").expect("start");

    let api_state = runtime.state("pidwrite").expect("state");
    let api_pid = api_state.pid.expect("pid from runtime.state");

    let state_json = std::fs::read(state_tmp.path().join("containers/pidwrite/state.json"))
        .expect("read state.json");
    let parsed: serde_json::Value = serde_json::from_slice(&state_json).expect("parse state.json");
    let disk_pid = parsed
        .get("pid")
        .and_then(|v| v.as_u64())
        .expect("pid field on disk");
    assert_eq!(
        disk_pid, api_pid as u64,
        "on-disk pid should match runtime.state"
    );

    let _ = runtime.kill("pidwrite", nix::sys::signal::Signal::SIGKILL);
    common::force_cleanup(&runtime, "pidwrite");
    common::teardown_cgroup_root(&cgroup_root);
}

#[test]
fn delete_force_false_errors_when_running() {
    if common::requires_root("delete_force_false_errors_when_running") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    let bundle =
        common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "sleep 5"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    runtime.create("hot", &bundle).expect("create");
    runtime.start("hot").expect("start");

    // Refuses without force.
    let err = runtime
        .delete("hot", false)
        .expect_err("delete should refuse a running container");
    let msg = format!("{err}");
    assert!(
        msg.contains("Running") || msg.contains("running"),
        "error should mention Running state, got: {msg}"
    );

    // Container still alive.
    let still = runtime.state("hot").expect("state after refused delete");
    assert_eq!(still.state, ContainerState::Running);

    // kill + force-delete cleans up. SIGKILL because PID 1 in a
    // fresh PID namespace ignores SIGTERM unless the entrypoint
    // installs a handler (busybox sleep does not).
    runtime
        .kill("hot", nix::sys::signal::Signal::SIGKILL)
        .expect("kill SIGKILL");
    let _ = common::wait_until_state(
        &runtime,
        "hot",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );
    runtime.delete("hot", true).expect("force delete");

    assert!(
        !state_tmp.path().join("containers/hot").exists(),
        "state-dir entry should be gone after force delete"
    );
    common::teardown_cgroup_root(&cgroup_root);
}

#[test]
fn kill_stops_container() {
    if common::requires_root("kill_stops_container") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    // We use SIGKILL rather than SIGTERM here because PID 1 in a
    // fresh PID namespace ignores signals it has no handler for
    // (kernel rule: PID 1 doesn't get a default SIGTERM disposition).
    // busybox `sh -c "sleep 30"` exec's into `sleep`, which has no
    // SIGTERM handler, so SIGTERM is silently dropped. SIGKILL is
    // unblockable and always reaps the process: that's what
    // `wisp delete --force` ultimately relies on too.
    let bundle =
        common::prepare_bundle(bundle_tmp.path(), &["/bin/sh", "-c", "sleep 30"], &[], None);
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    runtime.create("victim", &bundle).expect("create");
    runtime.start("victim").expect("start");
    assert_eq!(
        runtime.state("victim").expect("state").state,
        ContainerState::Running
    );

    runtime
        .kill("victim", nix::sys::signal::Signal::SIGKILL)
        .expect("kill SIGKILL");
    let final_state = common::wait_until_state(
        &runtime,
        "victim",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );
    assert_eq!(
        final_state,
        ContainerState::Stopped,
        "container should reach Stopped after SIGKILL"
    );

    runtime.delete("victim", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}

/// Phase 0.4 dispatch C1: child stdout / stderr land on
/// `<state_dir>/containers/<id>/{stdout,stderr}.log`. Run a busybox
/// `echo hi-from-stdout` and assert the log file picks it up.
#[test]
fn redirect_stdout_stderr_writes_to_file() {
    if common::requires_root("redirect_stdout_stderr_writes_to_file") {
        return;
    }
    let state_tmp = TempDir::new().unwrap();
    let bundle_tmp = TempDir::new().unwrap();
    let cgroup_root = common::unique_cgroup_root();

    let bundle = common::prepare_bundle(
        bundle_tmp.path(),
        &[
            "/bin/sh",
            "-c",
            "echo hi-from-stdout; echo hi-from-stderr 1>&2",
        ],
        &[],
        None,
    );
    let runtime = common::isolated_runtime(state_tmp.path(), &cgroup_root);

    runtime.create("logsdemo", &bundle).expect("create");
    runtime.start("logsdemo").expect("start");
    let _ = common::wait_until_state(
        &runtime,
        "logsdemo",
        ContainerState::Stopped,
        Duration::from_secs(5),
    );

    // The persisted handle records the log paths. Read both files
    // and assert each line landed on the expected stream.
    let handle = runtime.state("logsdemo").expect("state after stop");
    let stdout_path = handle
        .stdout_log_path
        .clone()
        .expect("stdout_log_path persisted");
    let stderr_path = handle
        .stderr_log_path
        .clone()
        .expect("stderr_log_path persisted");
    let stdout_bytes = std::fs::read(&stdout_path).expect("read stdout.log");
    let stderr_bytes = std::fs::read(&stderr_path).expect("read stderr.log");
    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    assert!(
        stdout_text.contains("hi-from-stdout"),
        "stdout.log should contain echo: got {stdout_text:?}"
    );
    assert!(
        stderr_text.contains("hi-from-stderr"),
        "stderr.log should contain echo: got {stderr_text:?}"
    );

    runtime.delete("logsdemo", true).expect("delete");
    common::teardown_cgroup_root(&cgroup_root);
}
