//! Integration tests for `isd configure ...` against the built binary.
//!
//! These cover help rendering and clap shape for the five sub-verbs. The
//! controller-talking paths are exercised against the dashboard's REST
//! surface in `dashboard/tests/config_endpoints.rs` (PR 2) and unit tests
//! in `crates/isd/src/configure.rs`. End-to-end is a manual smoke test
//! once PR 4 wires the call sites.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

/// Build a fresh `isd` invocation. Same helper shape as `ssh_cmd.rs`.
fn isd_bin() -> Command {
    Command::cargo_bin("isd").expect("isd binary built")
}

#[test]
fn configure_help_lists_subcommands() {
    let out = isd_bin()
        .args(["configure", "--help"])
        .output()
        .expect("run isd configure --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for verb in ["get", "set", "unset", "list", "schema"] {
        assert!(
            stdout.contains(verb),
            "configure --help mentions {verb}: {stdout}"
        );
    }
}

#[test]
fn configure_get_help_mentions_show_secret() {
    let out = isd_bin()
        .args(["configure", "get", "--help"])
        .output()
        .expect("run isd configure get --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--show-secret"),
        "get --help mentions --show-secret: {stdout}"
    );
}

#[test]
fn configure_set_help_mentions_stdin_and_from_file() {
    let out = isd_bin()
        .args(["configure", "set", "--help"])
        .output()
        .expect("run isd configure set --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--stdin"),
        "set --help mentions --stdin: {stdout}"
    );
    assert!(
        stdout.contains("--from-file"),
        "set --help mentions --from-file: {stdout}"
    );
}

#[test]
fn configure_set_refuses_inline_value_with_stdin() {
    // Parser-level conflict: clap should reject the combo without ever
    // touching the network.
    let out = isd_bin()
        .args(["configure", "set", "cloudflare.api_token", "v", "--stdin"])
        .output()
        .expect("run isd configure set with conflict");
    assert!(
        !out.status.success(),
        "expected non-zero exit on conflict, got success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("conflict") || combined.contains("cannot") || combined.contains("error"),
        "expected conflict diagnostic, got: {combined}"
    );
}

#[test]
fn configure_unset_help_takes_key() {
    let out = isd_bin()
        .args(["configure", "unset", "--help"])
        .output()
        .expect("run isd configure unset --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("<KEY>") || stdout.to_lowercase().contains("key"));
}

#[test]
fn configure_list_help_mentions_show_secrets() {
    let out = isd_bin()
        .args(["configure", "list", "--help"])
        .output()
        .expect("run isd configure list --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--show-secrets"),
        "list --help mentions --show-secrets: {stdout}"
    );
}

#[test]
fn configure_schema_is_a_recognized_verb() {
    let out = isd_bin()
        .args(["configure", "schema", "--help"])
        .output()
        .expect("run isd configure schema --help");
    assert!(
        out.status.success(),
        "schema --help should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn root_help_lists_configure_in_cluster_group() {
    let out = isd_bin().args(["--help"]).output().expect("run isd --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("configure"),
        "root --help mentions configure: {stdout}"
    );
    assert!(
        stdout.contains("Cluster"),
        "root --help has Cluster section: {stdout}"
    );
}
