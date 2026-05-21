//! Integration tests for `isd ssh ...` against the built binary.
//!
//! These tests cover the slices that do not require a live controller
//! (Session::open would refuse without a docker context). The
//! controller-talking paths (`mint`, `hosts`, `ca pubkey`) are
//! exercised against the dashboard's REST surface in
//! `dashboard/tests/ssh_endpoints.rs` and via the matching unit tests
//! in `crates/isd/src/ssh.rs`.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn isd_bin() -> Command {
    Command::cargo_bin("isd").expect("isd binary built")
}

#[test]
fn ssh_help_lists_subcommands() {
    let out = isd_bin()
        .args(["ssh", "--help"])
        .output()
        .expect("run isd ssh --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("mint"),
        "ssh --help mentions mint: {stdout}"
    );
    assert!(
        stdout.contains("status"),
        "ssh --help mentions status: {stdout}"
    );
    assert!(
        stdout.contains("hosts"),
        "ssh --help mentions hosts: {stdout}"
    );
    assert!(stdout.contains("ca"), "ssh --help mentions ca: {stdout}");
    assert!(
        stdout.contains("audit"),
        "ssh --help mentions audit: {stdout}"
    );
    assert!(
        stdout.contains("trust"),
        "ssh --help mentions trust: {stdout}"
    );
    assert!(
        stdout.contains("untrust"),
        "ssh --help mentions untrust: {stdout}"
    );
}

#[test]
fn ssh_trust_help_lists_bootstrap_flags() {
    let out = isd_bin()
        .args(["ssh", "trust", "--help"])
        .output()
        .expect("run isd ssh trust --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--user"),
        "trust --help mentions --user: {stdout}"
    );
    assert!(
        stdout.contains("--port"),
        "trust --help mentions --port: {stdout}"
    );
    assert!(
        stdout.contains("--no-record"),
        "trust --help mentions --no-record: {stdout}"
    );
}

#[test]
fn ssh_untrust_errors_when_host_not_in_store() {
    // No trusted_hosts.toml exists in the tmp HOME, so `untrust foo`
    // hits the "not in file" error path without ever touching the
    // network or a controller.
    let home = tempfile::tempdir().expect("tmp home");
    let out = isd_bin()
        .args(["ssh", "untrust", "nonexistent.example.invalid"])
        .env("HOME", home.path())
        .output()
        .expect("run isd ssh untrust");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not in") || stderr.contains("nothing to remove"),
        "stderr explains the host is absent: {stderr}"
    );
}

#[test]
fn ssh_audit_help_lists_filter_flags() {
    // Phase 6: `isd ssh audit` exposes `--since` and `--limit`.
    // The integration test sticks to `--help` so we never need a
    // live controller; the rendering path is covered by unit tests
    // in `crates/isd/src/ssh.rs`.
    let out = isd_bin()
        .args(["ssh", "audit", "--help"])
        .output()
        .expect("run isd ssh audit --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--since"),
        "audit --help mentions --since: {stdout}"
    );
    assert!(
        stdout.contains("--limit"),
        "audit --help mentions --limit: {stdout}"
    );
}

#[test]
fn root_help_groups_ssh_under_access() {
    let out = isd_bin().arg("--help").output().expect("run isd --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Access"),
        "root help has an Access group: {stdout}"
    );
    assert!(stdout.contains("ssh"), "root help lists ssh: {stdout}");
}

#[test]
fn ssh_status_errors_when_no_cert_in_fake_home() {
    // Point HOME at a tmp dir with no ~/.ssh so the cert lookup
    // returns "no cert" and the command exits non-zero with the
    // actionable hint.
    let home = tempfile::tempdir().expect("tmp home");
    let out = isd_bin()
        .args(["ssh", "status"])
        .env("HOME", home.path())
        .output()
        .expect("run isd ssh status");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no SSH cert") || stderr.contains("isd ssh mint"),
        "stderr suggests minting a cert: {stderr}"
    );
}

#[test]
fn ssh_status_errors_when_ssh_dir_empty() {
    // Same as the no-HOME-ssh-dir test, but with the directory
    // present and empty. Covers the `read_dir succeeds but no
    // matching file` branch.
    let home = tempfile::tempdir().expect("tmp home");
    std::fs::create_dir(home.path().join(".ssh")).expect("mkdir .ssh");
    let out = isd_bin()
        .args(["ssh", "status"])
        .env("HOME", home.path())
        .output()
        .expect("run isd ssh status");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no SSH cert") || stderr.contains("isd ssh mint"),
        "stderr suggests minting a cert: {stderr}"
    );
}

#[test]
fn ssh_external_subcommand_captures_host_arg() {
    // `isd ssh <host>` parses successfully (clap routes to the
    // external_subcommand). We deliberately do NOT let the dial
    // path execute (no controller, no docker context). Asking
    // for `--help` after the host would not work since
    // external_subcommand swallows all tokens. We assert here
    // by setting a bogus context so Session::open fails fast,
    // and verifying the failure mentions the docker/context layer
    // rather than a clap parse error.
    let _ = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = tempfile::tempdir().expect("tmp home");
    // Drop a stub pubkey + cert so cert_is_stale short-circuits
    // away from the auto-mint path (which would hit the network).
    let ssh_dir = home.path().join(".ssh");
    std::fs::create_dir(&ssh_dir).unwrap();
    std::fs::write(
        ssh_dir.join("id_ed25519.pub"),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabcdef test\n",
    )
    .unwrap();
    // Pre-shaped cert file present. cert_is_stale will still
    // consider it stale (ssh-keygen will fail to parse it), so the
    // dial path will try to auto-mint. That call to Session::open
    // will then fail because there is no docker context. We expect
    // a non-zero exit with a useful error message; the test asserts
    // we got past the clap parse step.
    std::fs::write(ssh_dir.join("id_ed25519-cert.pub"), "garbage").unwrap();
    let out = isd_bin()
        .args(["ssh", "nonexistent.example.invalid"])
        .env("HOME", home.path())
        // Force a context that cannot resolve so we fail fast.
        .args(["--context", "nonexistent-context-for-ssh-cmd-test"])
        .output()
        .expect("run isd ssh nonexistent.example.invalid");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Critically: this is NOT a clap usage error. Clap parse errors
    // start with `error:` from clap's UI. Our error is an isd:
    // wrapped anyhow chain.
    assert!(
        stderr.starts_with("isd:") || stderr.contains("context"),
        "expected runtime error, not a clap parse error: {stderr}"
    );
}
