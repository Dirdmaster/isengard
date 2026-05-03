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
