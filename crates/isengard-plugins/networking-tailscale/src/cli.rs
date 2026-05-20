//! Thin async wrappers around `tailscale` subprocess invocations.
//!
//! Every function shells out via `tokio::process::Command` and turns
//! non-zero exits into [`CoreError::Other`] with stderr appended. The
//! adapter never parses tailscale's human-readable output: it asks for
//! `--json` (status) or trusts the exit code (serve, funnel, cert).

use isengard_core::error::{CoreError, Result};
use serde::Deserialize;
use tokio::process::Command;

/// Verifies the `tailscale` binary is on `PATH`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] with an install link when `tailscale`
/// isn't found.
pub fn ensure_present() -> Result<()> {
    if which::which("tailscale").is_err() {
        return Err(CoreError::Other(
            "`tailscale` CLI not found in PATH; install from https://tailscale.com/download".into(),
        ));
    }
    Ok(())
}

/// Decoded subset of `tailscale status --json`.
///
/// Only the fields the adapter needs to decide "is the tailnet up?".
#[derive(Debug, Deserialize)]
pub struct TailscaleStatus {
    /// Tailscale's backend state machine label. `Running` means the
    /// daemon is fully up and routing.
    #[serde(rename = "BackendState")]
    pub backend_state: String,
    /// Convenience flag set by [`status`] when `backend_state ==
    /// "Running"`. Not present in the JSON itself.
    #[serde(default)]
    pub online: bool,
}

/// Runs `tailscale status --json` and parses the result.
///
/// Post-processes the parsed struct: sets `online = true` when
/// `backend_state == "Running"`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when the subprocess fails to launch,
/// exits non-zero, or returns JSON the [`TailscaleStatus`] decoder
/// rejects.
pub async fn status() -> Result<TailscaleStatus> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("running tailscale status: {e}")))?;

    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale status --json` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let mut status: TailscaleStatus = serde_json::from_slice(&out.stdout)
        .map_err(|e| CoreError::Other(format!("parsing tailscale status JSON: {e}")))?;

    status.online = status.backend_state == "Running";

    Ok(status)
}

/// Runs `tailscale serve --bg --https=443 --set-path=/ http://localhost:<local_port>`.
///
/// Wires the tailnet's port 443 to the local listener. The adapter calls
/// this once per `expose`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when the subprocess fails to launch or
/// exits non-zero.
pub async fn serve_https(local_port: u16) -> Result<()> {
    let out = Command::new("tailscale")
        .args([
            "serve",
            "--bg",
            "--https=443",
            "--set-path=/",
            &format!("http://localhost:{local_port}"),
        ])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale serve: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale serve` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Tears down the serve config on port 443 (path `/`).
///
/// See the limitation note on
/// [`TailscaleAdapter::unexpose`](crate::TailscaleAdapter::unexpose) for
/// why this is global per port.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when the subprocess fails or exits
/// non-zero.
pub async fn serve_off() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["serve", "--https=443", "--set-path=/", "off"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale serve off: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale serve off` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Flips `tailscale funnel` on for port 443.
///
/// Makes the hostname reachable from the public internet rather than
/// only from the tailnet. The adapter calls this when
/// `adapter_specific.funnel == true`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when the subprocess fails or exits
/// non-zero (e.g. the tailnet's ACL forbids funnel).
pub async fn funnel_on() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["funnel", "--bg", "--https=443", "on"])
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale funnel on: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale funnel on` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Flips `tailscale funnel` off for port 443. Best-effort.
///
/// Idempotent: turning funnel off when it was never on is allowed and
/// any error is swallowed. The caller (typically `unexpose`) never
/// needs to know whether funnel was active.
pub async fn funnel_off() -> Result<()> {
    let _ = Command::new("tailscale")
        .args(["funnel", "--https=443", "off"])
        .output()
        .await;
    Ok(())
}

/// Runs `tailscale cert <hostname>` and returns the issued PEM material.
///
/// Returns `(cert_pem, key_pem)` read from `<tmpdir>/<hostname>.crt` and
/// `<tmpdir>/<hostname>.key`. `hostname` is validated against a DNS-label
/// charset before being interpolated into the temp path.
///
/// File reads use `tokio::fs` so the call doesn't block the agent's
/// runtime. The cert is small (a few KB) but the runtime is shared with
/// the proxy's request hot path.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when `hostname` contains
/// path-traversal characters or non-DNS-label bytes, when the
/// subprocess fails or exits non-zero, or when the resulting PEM files
/// can't be read.
pub async fn fetch_cert(hostname: &str) -> Result<(String, String)> {
    validate_hostname(hostname)?;
    let tmp = tempfile::tempdir().map_err(|e| CoreError::Other(format!("tempdir: {e}")))?;

    let out = Command::new("tailscale")
        .args(["cert", hostname])
        .current_dir(tmp.path())
        .output()
        .await
        .map_err(|e| CoreError::Other(format!("tailscale cert: {e}")))?;
    if !out.status.success() {
        return Err(CoreError::Other(format!(
            "`tailscale cert {hostname}` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let cert = tokio::fs::read_to_string(tmp.path().join(format!("{hostname}.crt")))
        .await
        .map_err(|e| CoreError::Other(format!("reading cert: {e}")))?;
    let key = tokio::fs::read_to_string(tmp.path().join(format!("{hostname}.key")))
        .await
        .map_err(|e| CoreError::Other(format!("reading key: {e}")))?;
    Ok((cert, key))
}

/// Rejects hostnames that would escape the temp dir or break the
/// `tailscale cert` invocation.
///
/// Allows DNS labels: ASCII alphanumerics, `.`, and `-`. Refuses
/// empty input, slashes, backslashes, and `..`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when the hostname is empty, contains a
/// path-traversal character, or contains any byte outside the DNS-label
/// alphabet.
fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        return Err(CoreError::Other("hostname is empty".into()));
    }
    if hostname.contains('/') || hostname.contains('\\') || hostname.contains("..") {
        return Err(CoreError::Other(format!(
            "hostname {hostname:?} contains path-traversal characters"
        )));
    }
    if let Some(c) = hostname
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '-'))
    {
        return Err(CoreError::Other(format!(
            "hostname {hostname:?} contains invalid character {c:?}"
        )));
    }
    Ok(())
}
