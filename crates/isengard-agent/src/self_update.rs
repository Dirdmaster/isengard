//! Binary self-update for the systemd-native install (Phase 0.8).
//!
//! Replaces the docker-coupled rename-and-recreate flow in
//! `crates/isengard-plugins/updater/src/self_update.rs`. That code is
//! still around for the legacy docker-compose path; this module is the
//! systemd path.
//!
//! Flow:
//!   1. Download the new binary to a sibling `.new` file under the same
//!      directory as the running executable.
//!   2. Verify the sha256 matches the operator-supplied digest.
//!   3. `chmod 0755` and atomic-rename onto the running binary's path.
//!      On the same filesystem, `rename(2)` is atomic: readers either
//!      see the old inode or the new one, never a partial.
//!   4. Optionally trigger `systemctl restart iso-agent` (for the
//!      agent self-updating itself; the controller would be
//!      `iso-controller`). systemd's Type=simple unit re-execs the new
//!      binary on the next ExecStart.
//!
//! Why this is safe:
//!   - The atomic rename happens BEFORE the restart. If the rename
//!     fails (cross-fs, permissions), nothing has changed and the old
//!     binary keeps running.
//!   - `Type=simple` units don't carry a PID file, so systemd doesn't
//!     get confused by the process replacement.
//!   - The current process exits cleanly when systemd sends SIGTERM as
//!     part of the restart. The new binary's ExecStart picks up.
//!
//! What this does NOT do:
//!   - Talk to docker. Phase 0.8 hosts have no dockerd to talk to.
//!   - Update the controller. The controller's self-update lands later
//!     (Phase 0.10+); for now operators run install.sh again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// Maximum download size. Caps the buffer reqwest builds in memory so a
/// hostile / corrupt URL can't OOM the agent. The current `isengard`
/// musl binary is ~80 MiB; the limit is generous.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// One-shot self-update entry point.
///
/// `url` is the public download URL (typically a GitHub Releases asset).
/// `expected_sha256` is the lowercase-hex sha256 of the expected bytes,
/// matching the format the release pipeline writes to `<asset>.sha256`.
/// `restart_unit` is the systemd unit name to restart on success
/// (typically `iso-agent.service` for the agent updating itself, or
/// empty / `None` to skip the restart and let the caller orchestrate).
///
/// Returns `Ok(())` after the rename and (best-effort) restart.
/// Returns `Err` and changes nothing on disk if any step before the
/// rename fails. After the rename, restart errors are logged but
/// do not unwind the rename.
pub async fn run_self_update(
    url: &str,
    expected_sha256: &str,
    restart_unit: Option<&str>,
) -> Result<()> {
    let target = current_exe_path()?;
    info!(
        url,
        target = %target.display(),
        expected_sha256 = %expected_sha256,
        "self-update: starting"
    );

    let staging = staging_path(&target);
    download_and_verify(url, &staging, expected_sha256).await?;

    set_executable(&staging)?;
    atomic_replace(&staging, &target)
        .with_context(|| format!("renaming {staging:?} -> {target:?}"))?;
    info!(target = %target.display(), "self-update: binary replaced");

    if let Some(unit) = restart_unit {
        if let Err(e) = trigger_systemctl_restart(unit) {
            warn!(unit, error = %e, "self-update: systemctl restart failed (binary already replaced; manual restart required)");
        } else {
            info!(unit, "self-update: systemctl restart issued");
        }
    }

    Ok(())
}

/// Resolve the path of the currently-running executable. Wraps
/// `std::env::current_exe` in a `Result` with a friendlier error.
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("resolving current executable path for self-update")
}

/// Build the staging path: `<target>.new` next to the running binary.
/// Same directory guarantees the rename is on one filesystem (and thus
/// atomic).
fn staging_path(target: &Path) -> PathBuf {
    let mut p = target.to_path_buf();
    let mut name = p
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("isengard"));
    name.push(".new");
    p.set_file_name(name);
    p
}

/// Download `url` to `dest`, computing sha256 as we go. Verifies the
/// digest against `expected_sha256` (case-insensitive hex). Removes the
/// staging file on any error so a partial download doesn't sit around.
async fn download_and_verify(url: &str, dest: &Path, expected_sha256: &str) -> Result<()> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected_sha256 must be 64 hex characters, got {expected:?}");
    }

    let client = reqwest::Client::builder()
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-success status from {url}"))?;

    if let Some(len) = resp.content_length() {
        if len > MAX_DOWNLOAD_BYTES {
            bail!("download size {len} exceeds cap {MAX_DOWNLOAD_BYTES}; refusing to allocate");
        }
    }

    // Best-effort cleanup of any prior staging artifact before we
    // reopen. Avoids EEXIST on the rename later if the previous attempt
    // crashed mid-write.
    let _ = std::fs::remove_file(dest);

    let bytes = resp.bytes().await.context("reading response body")?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        bail!(
            "downloaded {} bytes; exceeds cap {MAX_DOWNLOAD_BYTES}",
            bytes.len()
        );
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got_hex = hex::encode(hasher.finalize());
    if got_hex != expected {
        bail!("sha256 mismatch: got {got_hex}, expected {expected}; refusing to install");
    }
    info!(bytes = bytes.len(), sha256 = %got_hex, "self-update: download verified");

    if let Err(e) = std::fs::write(dest, &bytes) {
        // Best-effort cleanup; the failure path is interesting enough
        // to log.
        let _ = std::fs::remove_file(dest);
        return Err(e).with_context(|| format!("writing staged binary to {dest:?}"));
    }
    Ok(())
}

/// chmod 0755 on Unix; no-op on other platforms (we don't ship those
/// for self-update, but the cfg keeps the test on Mac compilable).
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {path:?} for chmod"))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 0755 on {path:?}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Atomic rename on the same filesystem. Errors propagate verbatim; the
/// most likely failure modes are EXDEV (cross-filesystem) and EACCES.
fn atomic_replace(staging: &Path, target: &Path) -> Result<()> {
    std::fs::rename(staging, target).map_err(|e| {
        anyhow!(
            "atomic rename {} -> {} failed: {e} ({}). The staged binary is still at {}; \
             remove it manually after diagnosing.",
            staging.display(),
            target.display(),
            error_kind_hint(&e),
            staging.display()
        )
    })
}

/// Translate the most common rename(2) failure modes into a hint the
/// operator can act on. We can't always read the OS errno cross-platform,
/// so the kind() shorthand is best-effort.
fn error_kind_hint(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "permission denied; the rename target is not writable by this user"
        }
        std::io::ErrorKind::CrossesDevices => {
            "cross-filesystem rename; staging path must live on the same fs as the target"
        }
        std::io::ErrorKind::NotFound => "source file missing; download must have failed silently",
        _ => "see the error message",
    }
}

/// Run `systemctl restart <unit>`. Blocks the calling thread.
///
/// The current process is the one being restarted; once systemd sends
/// SIGTERM, this binary won't get to observe the exit code. We return
/// `Ok(())` if the spawn succeeded, on the theory that a non-zero exit
/// code from systemctl can still mean "I've started the restart" given
/// our own SIGTERM is mid-flight.
fn trigger_systemctl_restart(unit: &str) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["restart", unit])
        .status()
        .with_context(|| format!("spawning `systemctl restart {unit}`"))?;
    if status.success() {
        Ok(())
    } else {
        // Don't bail: the restart may have started even if systemctl
        // returned non-zero (e.g. it lost stderr to our own SIGTERM).
        warn!(
            unit,
            exit_code = ?status.code(),
            "systemctl restart returned non-zero; restart may have proceeded anyway"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_path_appends_new_suffix() {
        let p = Path::new("/usr/local/bin/isengard");
        let s = staging_path(p);
        assert_eq!(s, Path::new("/usr/local/bin/isengard.new"));
    }

    #[test]
    fn staging_path_handles_extensionless_filename() {
        let p = Path::new("/tmp/foo");
        let s = staging_path(p);
        assert_eq!(s, Path::new("/tmp/foo.new"));
    }

    #[test]
    fn staging_path_handles_path_with_extension() {
        // Even though we never ship .exe in our pipeline, exercise the
        // code path anyway. The staged file ends up `foo.exe.new`.
        let p = Path::new("/tmp/foo.exe");
        let s = staging_path(p);
        assert_eq!(s, Path::new("/tmp/foo.exe.new"));
    }

    #[tokio::test]
    async fn download_and_verify_rejects_short_digest() {
        let url = "http://localhost:1/never-reached";
        let dest = std::env::temp_dir().join("iso-self-update-short-digest");
        let _ = std::fs::remove_file(&dest);
        let res = download_and_verify(url, &dest, "abc").await;
        assert!(res.is_err(), "short digest should error before the request");
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("64 hex"),
            "error message should explain the format: {err}"
        );
        assert!(
            !dest.exists(),
            "no file should be staged on rejected digest"
        );
    }

    #[tokio::test]
    async fn download_and_verify_rejects_non_hex_digest() {
        let url = "http://localhost:1/never-reached";
        let dest = std::env::temp_dir().join("iso-self-update-non-hex-digest");
        let _ = std::fs::remove_file(&dest);
        // 64 chars but `g` is not hex.
        let bad = "g".repeat(64);
        let res = download_and_verify(url, &dest, &bad).await;
        assert!(res.is_err());
        assert!(!dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_executable_sets_0755() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("victim");
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        // Start with a non-executable mode.
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&p, perms).unwrap();
        set_executable(&p).expect("chmod");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755, "expected 0755, got {mode:o}");
    }

    #[test]
    fn atomic_replace_moves_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staging = dir.path().join("stage");
        let target = dir.path().join("target");
        std::fs::write(&staging, b"new contents").unwrap();
        atomic_replace(&staging, &target).expect("rename");
        assert!(!staging.exists(), "staging file should be gone");
        let body = std::fs::read(&target).unwrap();
        assert_eq!(body, b"new contents");
    }
}
