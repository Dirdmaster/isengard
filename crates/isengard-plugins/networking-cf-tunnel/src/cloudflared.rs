//! cloudflared subprocess supervisor. Mirrors `isengard-agent::proxy::supervise`
//! pattern: spawn, monitor, restart-with-backoff up to N times, give up.

use isengard_core::error::{CoreError, Result};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

const MAX_RESTARTS: u32 = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(300);

pub fn ensure_present() -> Result<()> {
    if which::which("cloudflared").is_err() {
        return Err(CoreError::Other(
            "`cloudflared` not found in PATH; install from https://github.com/cloudflare/cloudflared/releases"
                .into(),
        ));
    }
    Ok(())
}

pub async fn spawn(token: String) -> Result<Child> {
    Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "run", "--token", &token])
        .spawn()
        .map_err(|e| CoreError::Other(format!("spawning cloudflared: {e}")))
}

pub async fn supervise(token: String) {
    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        info!("cf-tunnel: starting cloudflared subprocess");
        let mut child = match spawn(token.clone()).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "cf-tunnel: failed to spawn cloudflared");
                return;
            }
        };

        let exit = child.wait().await;
        match exit {
            Ok(status) => warn!(?status, "cf-tunnel: cloudflared exited"),
            Err(e) => error!(error = %e, "cf-tunnel: waiting on cloudflared failed"),
        }

        let now = Instant::now();
        restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        restarts.push(now);
        if restarts.len() as u32 > MAX_RESTARTS {
            error!(
                restarts = restarts.len(),
                "cf-tunnel: restart budget exhausted; giving up (TODO: emit networking.adapter.crashloop)"
            );
            return;
        }
        let backoff = Duration::from_millis(250 * (1u64 << (restarts.len().min(5) as u64)));
        tokio::time::sleep(backoff).await;
    }
}
