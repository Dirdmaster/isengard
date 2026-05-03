//! Wrappers around `tokio::process::Command::new("tailscale")`.
//! Plan B 8f-1 covers `ensure_present` and `status`. `expose`-side wrappers
//! (`serve_https`, `funnel_on/off`, `fetch_cert`) land in PB-T16.

use isengard_core::error::{CoreError, Result};
use serde::Deserialize;
use tokio::process::Command;

pub fn ensure_present() -> Result<()> {
    if which::which("tailscale").is_err() {
        return Err(CoreError::Other(
            "`tailscale` CLI not found in PATH; install from https://tailscale.com/download".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    pub backend_state: String,
    #[serde(default)]
    pub online: bool,
}

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

    // BackendState is "Running" when the tailnet is up.
    status.online = status.backend_state == "Running";

    Ok(status)
}

/// Run `tailscale serve --bg --https=443 --set-path=/ http://localhost:<local_port>`.
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

pub async fn funnel_off() -> Result<()> {
    // Best-effort; ignore failure (idempotent off-call).
    let _ = Command::new("tailscale")
        .args(["funnel", "--https=443", "off"])
        .output()
        .await;
    Ok(())
}

/// Run `tailscale cert <hostname>` from a temp dir; returns (cert_pem, key_pem).
pub async fn fetch_cert(hostname: &str) -> Result<(String, String)> {
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

    let cert = std::fs::read_to_string(tmp.path().join(format!("{hostname}.crt")))
        .map_err(|e| CoreError::Other(format!("reading cert: {e}")))?;
    let key = std::fs::read_to_string(tmp.path().join(format!("{hostname}.key")))
        .map_err(|e| CoreError::Other(format!("reading key: {e}")))?;
    Ok((cert, key))
}
