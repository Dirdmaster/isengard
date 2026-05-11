//! End-to-end coverage for `isd update`.
//!
//! Three flows we want to lock down:
//!   1. `isd update --help` lists the documented flags.
//!   2. `isd update --check` (with `--version` pin equal to the dev
//!      build) prints the noop "already at" line without touching the
//!      filesystem or hitting the network.
//!   3. `isd update --yes --version <tag>` against a wiremock-backed
//!      GitHub stand-in: serves a sha256 manifest + a binary whose
//!      digest matches; verify the staged binary replaces a stub at
//!      `/tmp/test-isd-<pid>`. Marked `#[ignore]` so the cheap test
//!      loop doesn't pay the wiremock + hyper startup cost; opt-in via
//!      `cargo test -- --ignored`.
//!
//! The "binary actually gets installed" path inside `install_atomic` is
//! already exercised by the unit tests in `update_cmd`; the integration
//! suite verifies the HTTP wiring and CLI plumbing on top of that.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Tiny helper: spin up a MockServer, return it. The MockServer drops
/// at end of scope; wiremock tears the hyper task down on drop.
async fn start_mock() -> MockServer {
    MockServer::start().await
}

/// Construct a fake `LatestRelease` JSON response. We only deserialise
/// `tag_name`, but matching the real shape makes the test less
/// brittle if we later parse additional fields.
fn latest_release_body(tag: &str) -> serde_json::Value {
    serde_json::json!({
        "tag_name": tag,
        "name": tag,
        "draft": false,
        "prerelease": false,
    })
}

/// Compute the lowercase-hex sha256 of `bytes`. Mirrors the digest
/// check inside `update_cmd::download_and_verify`, so the manifest the
/// test serves matches the bytes the test serves.
fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Map the host's OS+arch to the asset name the binary will build.
/// Mirrors `update_cmd::asset_name` + `detect_target_triple_for`.
/// Returning `None` skips the test on unsupported hosts (e.g. CI's
/// FreeBSD runners, if any ever appear).
fn host_asset_name() -> Option<String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        _ => return None,
    };
    Some(format!("isd-{triple}"))
}

#[tokio::test]
async fn update_help_lists_documented_flags() {
    // No mock needed: clap renders help locally.
    Command::cargo_bin("isd")
        .unwrap()
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"))
        .stdout(predicate::str::contains("--version"))
        .stdout(predicate::str::contains("--yes"));
}

#[tokio::test]
async fn update_check_prints_current_and_latest() {
    let server = start_mock().await;
    Mock::given(method("GET"))
        .and(path("/repos/Weavers-Engineering/Isengard/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(latest_release_body("v99.0.0")))
        .mount(&server)
        .await;

    let assert = Command::cargo_bin("isd")
        .unwrap()
        .env("ISD_UPDATE_GITHUB_API", server.uri())
        .args(["update", "--check"])
        .assert()
        .success();
    assert
        .stdout(predicate::str::contains("current:"))
        .stdout(predicate::str::contains("latest:"))
        .stdout(predicate::str::contains("v99.0.0"));
}

#[tokio::test]
async fn update_check_rate_limit_surfaces_friendly_error() {
    let server = start_mock().await;
    Mock::given(method("GET"))
        .and(path("/repos/Weavers-Engineering/Isengard/releases/latest"))
        .respond_with(ResponseTemplate::new(403).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let assert = Command::cargo_bin("isd")
        .unwrap()
        .env("ISD_UPDATE_GITHUB_API", server.uri())
        .args(["update", "--check"])
        .assert()
        .failure();
    assert
        .stderr(predicate::str::contains("rate-limited").or(predicate::str::contains("--version")));
}

#[tokio::test]
async fn update_check_with_pinned_dev_version_short_circuits() {
    // The dev build advertises CARGO_PKG_VERSION = "0.1.0-alpha".
    // Pinning to that same tag must short-circuit on the equal-version
    // check, without hitting the network. We point the API URL at an
    // unreachable port to prove the API isn't queried.
    let assert = Command::cargo_bin("isd")
        .unwrap()
        .env("ISD_UPDATE_GITHUB_API", "http://127.0.0.1:1")
        .args(["update", "--check", "--version", "v0.1.0-alpha"])
        .assert()
        .success();
    assert.stdout(predicate::str::contains("already at"));
}

/// End-to-end happy path: serve a sha256 manifest + matching binary
/// from wiremock, point `isd update` at it, verify a stub binary at
/// `/tmp/test-isd-<pid>` gets replaced with the served bytes.
///
/// `#[ignore]` so the cheap test loop skips wiremock + hyper startup;
/// opt-in via `cargo test -- --ignored`.
#[tokio::test]
#[ignore]
async fn update_yes_replaces_stub_binary_when_sha_matches() {
    let Some(asset) = host_asset_name() else {
        eprintln!("skipping: no asset name mapped for this host");
        return;
    };
    let server = start_mock().await;

    // Stand up a stub "current binary" that the test will run. The
    // real `isd` binary uses `std::env::current_exe()` to find its
    // install path; assert_cmd launches the build artifact under
    // `target/debug/isd`, so we can't redirect that easily. We instead
    // copy the built binary to a tempdir and launch from there: then
    // `current_exe()` resolves inside the tempdir and the test owns
    // the rename target.
    let scratch = tempfile::tempdir().expect("tempdir");
    let original = assert_cmd::cargo::cargo_bin("isd");
    let runner = scratch.path().join("isd-runner");
    fs::copy(&original, &runner).expect("copy built isd into tempdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&runner).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&runner, perms).unwrap();
    }

    // Bytes the manifest will commit to + the binary will serve.
    let new_bytes: &[u8] = b"new-isd-binary-bytes-from-wiremock";
    let new_hex = hex_sha256(new_bytes);

    let tag = "v999.0.0";

    Mock::given(method("GET"))
        .and(path(format!(
            "/Weavers-Engineering/Isengard/releases/download/{tag}/{asset}.sha256"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{new_hex}  {asset}\n")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/Weavers-Engineering/Isengard/releases/download/{tag}/{asset}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(new_bytes.to_vec()))
        .mount(&server)
        .await;

    let assert = std::process::Command::new(&runner)
        .env("ISD_UPDATE_GITHUB_DOWNLOAD", server.uri())
        .args(["update", "--version", tag, "--yes"])
        .output()
        .expect("spawn runner isd");

    let stdout = String::from_utf8_lossy(&assert.stdout);
    let stderr = String::from_utf8_lossy(&assert.stderr);
    assert!(
        assert.status.success(),
        "expected success; stdout={stdout} stderr={stderr}"
    );

    // The runner binary should now be the served bytes.
    let installed = fs::read(&runner).expect("read installed runner");
    assert_eq!(
        installed, new_bytes,
        "runner binary was not replaced by the served bytes"
    );

    // The staging path should be gone after a successful install.
    let staging = scratch.path().join("isd-runner.new");
    assert!(!staging.exists(), "staging file {staging:?} not cleaned up");
}

/// SHA mismatch path: same wiremock setup, but the binary's bytes
/// disagree with the manifest. The runner must refuse to install and
/// leave the stub binary untouched.
#[tokio::test]
#[ignore]
async fn update_yes_aborts_on_sha_mismatch() {
    let Some(asset) = host_asset_name() else {
        eprintln!("skipping: no asset name mapped for this host");
        return;
    };
    let server = start_mock().await;

    let scratch = tempfile::tempdir().expect("tempdir");
    let original = assert_cmd::cargo::cargo_bin("isd");
    let runner = scratch.path().join("isd-runner");
    fs::copy(&original, &runner).expect("copy built isd");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&runner).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&runner, perms).unwrap();
    }
    let original_bytes = fs::read(&runner).expect("read runner pre-update");

    // Manifest commits to one byte sequence; binary serves a different
    // one. download_and_verify must reject before any rename.
    let promised: &[u8] = b"expected-bytes";
    let promised_hex = hex_sha256(promised);
    let tampered: &[u8] = b"tampered-bytes";

    let tag = "v999.0.0";

    Mock::given(method("GET"))
        .and(path(format!(
            "/Weavers-Engineering/Isengard/releases/download/{tag}/{asset}.sha256"
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{promised_hex}  {asset}\n")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/Weavers-Engineering/Isengard/releases/download/{tag}/{asset}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tampered.to_vec()))
        .mount(&server)
        .await;

    let output = std::process::Command::new(&runner)
        .env("ISD_UPDATE_GITHUB_DOWNLOAD", server.uri())
        .args(["update", "--version", tag, "--yes"])
        .output()
        .expect("spawn runner isd");

    assert!(
        !output.status.success(),
        "expected failure on sha mismatch; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("sha256"),
        "expected sha256 error in stderr; got: {stderr}"
    );

    // The runner binary must not have been touched.
    let after = fs::read(&runner).expect("read runner post-update");
    assert_eq!(
        after, original_bytes,
        "runner binary was modified despite sha mismatch"
    );

    // Staging file (if any) should not be left behind on the failure
    // path. download_and_verify's error branch unlinks it.
    let staging = scratch.path().join("isd-runner.new");
    assert!(
        !staging.exists(),
        "staging file {staging:?} left behind after sha mismatch"
    );
}
