//! Integration test: spawns the `isengard` binary, asserts plugin loading +
//! CLI surface., controller mode runs a long-lived gRPC server,
//! so its plugin-loading proof now lives in
//! `crates/isengard-controller/tests/server_skeleton.rs`. Agent mode still
//! returns immediately (wires the long-lived agent loop).

use assert_cmd::Command;
use predicates::prelude::*;

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

#[test]
fn token_mint_help_lists_join_command_flags() {
    let assert = Command::cargo_bin("isengard")
        .unwrap()
        .args(["controller", "token", "mint", "--help"])
        .assert()
        .success();
    assert
        .stdout(predicate::str::contains("--public-addr"))
        .stdout(predicate::str::contains("--image"))
        .stdout(predicate::str::contains("--format"));
}
