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
//!   4. Replace each named unit via an explicit `stop -> wait inactive
//!      -> wait listening port free -> start` cycle (NOT `systemctl
//!      restart`). The `restart` shortcut returns the moment the new
//!      ExecStart has been spawned, which can be BEFORE the old
//!      process has released its listening socket. Pingora 0.8 owns
//!      the socket inside its accept-loop task and only drops it when
//!      its graceful-shutdown broadcast completes; the new process
//!      hits `EADDRINUSE` on bind and panics. The graceful_replace
//!      sequence below polls the unit's ActiveState and then probes
//!      each known listener port until it is free.
//!
//! Why this is safe:
//!   - The atomic rename happens BEFORE any unit stop. If the rename
//!     fails (cross-fs, permissions), nothing has changed and the old
//!     binary keeps running.
//!   - `Type=simple` units don't carry a PID file, so systemd doesn't
//!     get confused by the process replacement.
//!   - The current process exits cleanly when systemd sends SIGTERM as
//!     part of the stop. The new binary's ExecStart picks up on start.
//!
//! What this does NOT do:
//!   - Talk to docker. Phase 0.8 hosts have no dockerd to talk to.
//!   - Update the controller. The controller's self-update lands later
//!     (Phase 0.10+); for now operators run install.sh again.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// Max time to wait for a unit to reach inactive after `systemctl stop`.
/// systemd itself caps the SIGTERM->SIGKILL window via TimeoutStopSec on
/// the unit (15s on our iso-agent unit; see install/systemd/). 60s gives
/// systemd's own kill cascade time to land before we fall back to a
/// manual `systemctl kill --signal=SIGKILL`.
const STOP_WAIT_DEADLINE: Duration = Duration::from_secs(60);

/// Max time to wait for a previously-bound port to be free after the unit
/// has reached inactive. Most stops release the FD inside a few hundred
/// milliseconds; 30s is the safety net for `TIME_WAIT` style lingerings
/// (which our config avoids by NOT setting `SO_LINGER` with a non-zero
/// timeout, but a hostile environment could still hold it).
const PORT_FREE_DEADLINE: Duration = Duration::from_secs(30);

/// Pause between polls. 200ms is small enough that a fast stop only
/// costs one or two ticks of latency, and large enough that we don't
/// busy-loop the host while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Pause after `systemctl start` before checking that the unit is in
/// `active` state. Pingora's bootstrap + bind happens inside a couple
/// hundred ms; 2s leaves slack for slow startups (e.g. ACME bootstrap).
const POST_START_VERIFY_DELAY: Duration = Duration::from_secs(2);

/// Maximum download size. Caps the buffer reqwest builds in memory so a
/// hostile / corrupt URL can't OOM the agent. The current `isengard`
/// musl binary is ~80 MiB; the limit is generous.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// One-shot self-update entry point.
///
/// `url` is the public download URL (typically a GitHub Releases asset).
/// `expected_sha256` is the lowercase-hex sha256 of the expected bytes,
/// matching the format the release pipeline writes to `<asset>.sha256`.
/// `restart_units` is the list of systemd unit names to cycle on
/// success. Pass `&["iso-agent.service"]` for the agent updating itself,
/// `&["iso-controller.service", "iso-agent.service"]` for the operator
/// driving `isengard update`, or `&[]` to skip the cycle and let the
/// caller orchestrate.
///
/// Each unit is cycled via [`graceful_replace`]: explicit
/// `stop -> wait inactive -> wait listening ports free -> start`.
/// This is NOT `systemctl restart`, which returned before the old
/// Pingora process released its listener and made the new ExecStart
/// hit `EADDRINUSE` (lausanne v0.5.2 deploy, 2026-05-10).
///
/// Units are cycled in order. A failure on one unit aborts the chain
/// to avoid leaving systemd in a half-cycled state.
///
/// Returns `Ok(())` after the rename and unit cycles.
/// Returns `Err` and changes nothing on disk if any step before the
/// rename fails. After the rename, an error from the cycle is surfaced
/// but the new binary is already on disk; the operator's escape hatch
/// is `sudo systemctl start <unit>` for the failed unit.
pub async fn run_self_update(
    url: &str,
    expected_sha256: &str,
    restart_units: &[&str],
) -> Result<()> {
    let target = current_exe_path()?;
    info!(
        url,
        target = %target.display(),
        expected_sha256 = %expected_sha256,
        units = ?restart_units,
        "self-update: starting"
    );

    let staging = staging_path(&target);
    download_and_verify(url, &staging, expected_sha256).await?;

    set_executable(&staging)?;
    atomic_replace(&staging, &target)
        .with_context(|| format!("renaming {staging:?} -> {target:?}"))?;
    info!(target = %target.display(), "self-update: binary replaced");

    for unit in restart_units {
        if unit.is_empty() {
            continue;
        }
        if let Err(e) = graceful_replace(unit) {
            warn!(unit, error = %e, "self-update: graceful_replace failed (binary already on disk; run `sudo systemctl start {unit}` to recover)");
            return Err(e).with_context(|| format!("graceful_replace {unit}"));
        }
        info!(unit, "self-update: unit cycled");
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

/// Explicit `stop -> wait inactive -> wait listening port free -> start`
/// cycle for a single systemd unit.
///
/// Replaces the previous `systemctl restart <unit>` shortcut. `restart`
/// returned the moment the new ExecStart was queued; the old process's
/// Pingora listener was still bound to `0.0.0.0:8080` inside its
/// graceful-shutdown window (default 300s grace_period_seconds) and the
/// new agent's bind retried for ~7 seconds before panicking with
/// `Address in use`.
///
/// The sequence:
///   1. `systemctl stop <unit>`. Sends SIGTERM; systemd's
///      TimeoutStopSec eventually escalates to SIGKILL.
///   2. Poll `systemctl show --property ActiveState <unit>` every 200ms
///      until it reports `inactive` or `failed`. If the deadline passes
///      we send `systemctl kill --signal=SIGKILL <unit>` and continue.
///   3. Probe each port the unit owns (lookup table) by attempting a
///      bind. If the bind succeeds the port is free; close immediately.
///   4. `systemctl start <unit>`.
///   5. Sleep `POST_START_VERIFY_DELAY` and confirm `ActiveState=active`.
///
/// Blocks the calling thread. `isengard update` is interactive and the
/// caller already prints a plan + confirm, so a few seconds of synchronous
/// blocking is the right cost trade.
fn graceful_replace(unit: &str) -> Result<()> {
    info!(unit, "graceful_replace: stop");
    run_systemctl(&["stop", unit]).with_context(|| format!("systemctl stop {unit}"))?;

    // Step 2: wait for ActiveState to leave `activating` / `deactivating`.
    let deadline = Instant::now() + STOP_WAIT_DEADLINE;
    loop {
        let state = systemctl_show_property(unit, "ActiveState")?;
        info!(unit, state = %state, "graceful_replace: poll ActiveState");
        if state == "inactive" || state == "failed" {
            break;
        }
        if Instant::now() >= deadline {
            warn!(
                unit,
                state = %state,
                "graceful_replace: unit still not inactive after deadline; sending SIGKILL"
            );
            // Best-effort: ignore the exit status because the unit may
            // already be inactive by the time `kill` runs.
            let _ = run_systemctl(&["kill", "--signal=SIGKILL", unit]);
            std::thread::sleep(Duration::from_secs(2));
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Step 3: probe ports. We hard-code the table because Type=simple
    // units don't expose their listened ports to systemd (socket
    // activation would, but we don't use that).
    for port in ports_for_unit(unit) {
        wait_for_port_free(port);
    }

    // Step 4: start the new instance.
    info!(unit, "graceful_replace: start");
    run_systemctl(&["start", unit]).with_context(|| format!("systemctl start {unit}"))?;

    // Step 5: sanity-check that the start actually landed. systemd's
    // `start` returns the moment ExecStart is queued, not when the
    // process is running; on Type=simple that's "now" but a bind
    // failure or panic would flip the state to `failed` shortly after.
    std::thread::sleep(POST_START_VERIFY_DELAY);
    let state = systemctl_show_property(unit, "ActiveState")?;
    if state != "active" {
        bail!(
            "unit {unit} reached state {state} after start (expected `active`); inspect with `journalctl -u {unit}`"
        );
    }
    info!(unit, "graceful_replace: unit active");
    Ok(())
}

/// Lookup table: the listening ports for each unit we know about.
///
/// Phase 0.8 doesn't introspect the running config (the agent reads
/// HTTP_PORT / HTTPS_PORT from an env file, the controller from CLI
/// flags). The defaults shipped in `install/systemd/*.service` are:
///   - iso-agent     : 8080 (HTTP), 8443 (HTTPS)
///   - iso-controller: 9417 (gRPC), 9418 (HTTP UI)
///
/// Accepts both the bare unit name (`iso-agent`) and the full
/// `iso-agent.service` form callers use.
pub(crate) fn ports_for_unit(unit: &str) -> Vec<u16> {
    let bare = unit.trim_end_matches(".service");
    match bare {
        "iso-controller" => vec![9417, 9418],
        "iso-agent" => vec![8080, 8443],
        _ => vec![],
    }
}

/// Poll `port` until either:
///   - we can bind it on `0.0.0.0` (port is free), or
///   - [`PORT_FREE_DEADLINE`] elapses.
///
/// The bind probe uses `TcpListener::bind`; on success the listener is
/// dropped immediately and the port returns to the free pool. We
/// deliberately do NOT set `SO_REUSEADDR`: the production listener (the
/// new agent's Pingora server) doesn't either, so testing with the same
/// semantics is the only honest check.
fn wait_for_port_free(port: u16) {
    let deadline = Instant::now() + PORT_FREE_DEADLINE;
    loop {
        if !port_in_use(port) {
            info!(port, "graceful_replace: port is free");
            return;
        }
        if Instant::now() >= deadline {
            warn!(
                port,
                "graceful_replace: port still in use after deadline; starting unit anyway"
            );
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Try to bind `port` on `0.0.0.0`; if the bind fails the port is in
/// use by another process. Drops the listener immediately on success.
///
/// `pub(crate)` for the unit tests that need to exercise this against a
/// known-bound port.
pub(crate) fn port_in_use(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_err()
}

/// Run `systemctl <args>` and bail on a non-zero exit. Captures
/// stdout/stderr so the operator sees the systemd error in the
/// `bail!` message.
fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("spawning `systemctl {args:?}`"))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "systemctl {args:?} failed: {}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
            out.status
        );
    }
    Ok(())
}

/// Read a single property off a unit via `systemctl show --property
/// <prop> --value <unit>`. Trims trailing whitespace.
///
/// `--value` collapses the output to the bare value (no `Property=`
/// prefix), which keeps the parse trivial. Available since systemd 230
/// (Ubuntu 16.10+); every supported Phase 0.8 host has it.
pub(crate) fn systemctl_show_property(unit: &str, prop: &str) -> Result<String> {
    let out = std::process::Command::new("systemctl")
        .args(["show", "--property", prop, "--value", unit])
        .output()
        .with_context(|| format!("spawning `systemctl show --property {prop} {unit}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "systemctl show --property {prop} {unit} failed: {}\nstderr: {stderr}",
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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

    #[test]
    fn ports_for_unit_known_units() {
        assert_eq!(ports_for_unit("iso-controller.service"), vec![9417, 9418]);
        assert_eq!(ports_for_unit("iso-controller"), vec![9417, 9418]);
        assert_eq!(ports_for_unit("iso-agent.service"), vec![8080, 8443]);
        assert_eq!(ports_for_unit("iso-agent"), vec![8080, 8443]);
    }

    #[test]
    fn ports_for_unit_unknown_unit_returns_empty() {
        assert!(ports_for_unit("docker.service").is_empty());
        assert!(ports_for_unit("").is_empty());
        assert!(ports_for_unit("some-other.service").is_empty());
    }

    #[test]
    fn port_in_use_free_port_returns_false() {
        // Bind a listener, read back the OS-assigned port, drop it, and
        // confirm `port_in_use` reports false. The race window between
        // drop + re-probe is tiny but non-zero; pick a fresh ephemeral
        // each time to keep it low.
        let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
        let port = l.local_addr().unwrap().port();
        drop(l);
        // Tiny sleep to give the kernel a tick to release the FD on the
        // off chance the runner is under load. 50ms is generous.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!port_in_use(port), "port {port} should be free after drop");
    }

    #[test]
    fn port_in_use_bound_port_returns_true() {
        // Hold a listener on the same address+port that `port_in_use`
        // attempts to bind. The probe must observe EADDRINUSE and
        // report true.
        let l = TcpListener::bind(("0.0.0.0", 0)).expect("bind ephemeral");
        let port = l.local_addr().unwrap().port();
        // Keep `l` alive across the probe; dropping early would defeat
        // the test.
        assert!(
            port_in_use(port),
            "port {port} should report as in-use while listener held"
        );
        drop(l);
    }
}
