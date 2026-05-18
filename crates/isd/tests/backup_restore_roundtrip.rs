//! Round-trip integration test for `isd backup` + `isd restore` against a
//! real local docker. The test:
//!
//!   1. Creates a fresh docker volume named `iso-controller-state` (the
//!      backup pipeline's hard-coded source) seeded with a known marker file.
//!   2. Runs `isd backup --out <tmp>/back.tgz.age` with a known passphrase.
//!   3. Wipes the volume (and the operator's host-side backup.toml).
//!   4. Runs `isd restore <tmp>/back.tgz.age --overwrite`.
//!   5. Asserts the marker file is back inside the volume.
//!
//! Marked `#[ignore]`; runs only via `cargo test -- --ignored`. Requires a
//! real docker daemon reachable through the local context. The test reads
//! the docker URI from `DOCKER_HOST` env or falls back to
//! `unix:///var/run/docker.sock`. We avoid hitting the operator's real
//! `~/.config/isd/credentials.toml`: we spawn a temp HOME and seed
//! `~/.config/isd/credentials.toml` with a one-off context.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const STATE_VOLUME: &str = "iso-controller-state";
const TEST_CONTEXT: &str = "backup-roundtrip-test";
const PASSPHRASE: &str = "roundtrip-passphrase";
const MARKER_FILENAME: &str = "marker.txt";
const MARKER_BODY: &str = "iso-controller-state survived the round trip";

fn docker_uri() -> String {
    std::env::var("DOCKER_HOST").unwrap_or_else(|_| "unix:///var/run/docker.sock".into())
}

/// Run a one-shot docker container synchronously via the docker CLI. The
/// integration test deliberately shells out instead of using bollard so its
/// preflight (volume create, marker seed, wipe, marker check) does not share
/// implementation details with the code under test.
fn docker_cli(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("docker");
    cmd.env("DOCKER_HOST", docker_uri());
    cmd.args(args);
    cmd.output()
}

fn assert_docker_ok(args: &[&str]) {
    let out = docker_cli(args).expect("docker CLI invocation");
    if !out.status.success() {
        panic!(
            "docker {} failed: stdout={} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn seed_volume_with_marker() {
    // Wipe any prior content first so the test is repeatable.
    let _ = docker_cli(&["volume", "rm", "-f", STATE_VOLUME]);
    assert_docker_ok(&["volume", "create", STATE_VOLUME]);
    let cmd = format!("echo -n {MARKER_BODY:?} > /state/{MARKER_FILENAME}");
    assert_docker_ok(&[
        "run",
        "--rm",
        "-v",
        &format!("{STATE_VOLUME}:/state"),
        "alpine:3.21",
        "sh",
        "-c",
        &cmd,
    ]);
}

fn wipe_volume() {
    assert_docker_ok(&[
        "run",
        "--rm",
        "-v",
        &format!("{STATE_VOLUME}:/state"),
        "alpine:3.21",
        "sh",
        "-c",
        "rm -rf /state/* /state/.[!.]* /state/..?*",
    ]);
}

fn assert_marker_restored() {
    let out = docker_cli(&[
        "run",
        "--rm",
        "-v",
        &format!("{STATE_VOLUME}:/state:ro"),
        "alpine:3.21",
        "cat",
        &format!("/state/{MARKER_FILENAME}"),
    ])
    .expect("docker cat marker");
    if !out.status.success() {
        panic!(
            "marker not restored: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let body = String::from_utf8_lossy(&out.stdout);
    // `echo -n "..."` writes the quoted body verbatim. Tolerate trailing
    // newlines defensively.
    assert!(
        body.contains(MARKER_BODY),
        "marker body mismatch. expected to contain {MARKER_BODY:?}, got {body:?}"
    );
}

/// Write the controller-context creds and the backup creds into a temp
/// home. `isd` looks at `ISD_CREDENTIALS_FILE` (for the controller-context
/// file) and `dirs::home_dir()` (for `~/.config/isd/backup.toml`). We
/// override both via env so the test never touches the real operator
/// state.
fn write_context_file(home: &std::path::Path, docker_uri: &str) -> PathBuf {
    let cfg_path = home.join("credentials.toml");
    let creds = format!(
        r#"default_context = "{TEST_CONTEXT}"

[[contexts]]
name = "{TEST_CONTEXT}"
kind = "docker"
url = "{docker_uri}"
"#,
    );
    std::fs::write(&cfg_path, creds).unwrap();
    cfg_path
}

#[test]
#[ignore = "requires a running local docker daemon"]
fn backup_restore_roundtrip_fs() {
    // Skip if `docker` isn't on PATH so CI without docker doesn't trip.
    if which::which("docker").is_err() {
        eprintln!("docker CLI not on PATH; skipping");
        return;
    }
    // Sanity ping: if the daemon isn't reachable, skip with a clear msg.
    let ping = docker_cli(&["version", "--format", "{{.Server.Version}}"]).unwrap();
    if !ping.status.success() {
        eprintln!(
            "docker daemon not reachable ({}); skipping",
            String::from_utf8_lossy(&ping.stderr).trim()
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().to_path_buf();
    let backup_path = tmp.path().join("back.tgz.age");
    let creds_path = write_context_file(tmp.path(), &docker_uri());

    seed_volume_with_marker();

    // 1. isd backup --out <tmp>/back.tgz.age
    let status = std::process::Command::cargo_bin("isd")
        .expect("isd binary")
        .arg("backup")
        .arg("--out")
        .arg(&backup_path)
        .env("HOME", &home)
        .env("ISD_CREDENTIALS_FILE", &creds_path)
        .env("ISENGARD_BACKUP_PASSPHRASE", PASSPHRASE)
        .env("DOCKER_HOST", docker_uri())
        .status()
        .expect("isd backup");
    assert!(status.success(), "isd backup failed with {status}");
    let meta = std::fs::metadata(&backup_path).expect("backup file exists");
    assert!(meta.len() > 0, "backup archive is empty");

    // 2. Wipe the volume.
    wipe_volume();

    // 3. isd restore <tmp>/back.tgz.age --overwrite
    //    (--overwrite because, even after wipe, the bind mount may show an
    //    empty volume as populated on some docker storage drivers when
    //    they retain lost+found. --overwrite is the safe default here.)
    let status = std::process::Command::cargo_bin("isd")
        .expect("isd binary")
        .arg("restore")
        .arg(&backup_path)
        .arg("--overwrite")
        .env("HOME", &home)
        .env("ISD_CREDENTIALS_FILE", &creds_path)
        .env("ISENGARD_BACKUP_PASSPHRASE", PASSPHRASE)
        .env("DOCKER_HOST", docker_uri())
        .status()
        .expect("isd restore");
    assert!(status.success(), "isd restore failed with {status}");

    // 4. Marker is back.
    assert_marker_restored();

    // 5. Teardown.
    let _ = docker_cli(&["volume", "rm", "-f", STATE_VOLUME]);
}
