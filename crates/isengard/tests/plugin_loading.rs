//! Integration test: spawns the `isengard` binary in each mode, asserts the
//! `dev` plugin is loaded and lifecycled.

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
fn controller_mode_loads_dev_plugin() {
    let output = Command::cargo_bin("isengard")
        .unwrap()
        .args(["--log=info", "controller"])
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
