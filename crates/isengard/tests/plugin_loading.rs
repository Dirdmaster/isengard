//! Integration test: spawns the `isengard` binary, asserts plugin loading +
//! CLI surface. As of Phase 2a, controller mode runs a long-lived gRPC server,
//! so its plugin-loading proof now lives in
//! `crates/isengard-controller/tests/server_skeleton.rs`. Agent mode still
//! returns immediately (Phase 2d wires the long-lived agent loop).

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn agent_mode_loads_dev_plugin() {
    let output = Command::cargo_bin("isengard")
        .unwrap()
        .args(["--log=info", "agent"])
        .assert()
        .success();

    output.stderr(predicate::str::contains("plugin_count=1"));
}

#[test]
fn agent_help_lists_controller_flag() {
    Command::cargo_bin("isengard")
        .unwrap()
        .args(["agent", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--controller"));
}

#[test]
fn controller_help_lists_listen_flag() {
    Command::cargo_bin("isengard")
        .unwrap()
        .args(["controller", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--listen"));
}
