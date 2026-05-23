//! `cloudflared` subprocess supervisor.
//!
//! Mirrors the agent's proxy supervisor pattern: spawn, monitor, restart
//! with backoff up to a budget, then give up. The supervised process
//! holds the persistent edge connection to Cloudflare; when it dies the
//! tunnel goes down with it.

use isengard_core::error::{CoreError, Result};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

/// Maximum number of restarts allowed inside `RESTART_WINDOW` before
/// the supervisor gives up.
const MAX_RESTARTS: u32 = 5;

/// Rolling window over which restart attempts count against `MAX_RESTARTS`.
const RESTART_WINDOW: Duration = Duration::from_secs(300);

/// Verifies the `cloudflared` binary is on `PATH`.
///
/// # Errors
///
/// Returns [`CoreError::Other`] with an install link when `cloudflared`
/// isn't found.
pub fn ensure_present() -> Result<()> {
    if which::which("cloudflared").is_err() {
        return Err(CoreError::Other(
            "`cloudflared` not found in PATH; install from https://github.com/cloudflare/cloudflared/releases"
                .into(),
        ));
    }
    Ok(())
}

/// Spawns one `cloudflared tunnel run --token <t>` child process.
///
/// `--no-autoupdate` is set so the binary the operator installed is the
/// binary that runs: the supervisor wants predictable behavior, not
/// surprise version drift mid-deploy.
///
/// # Errors
///
/// Returns [`CoreError::Other`] when `tokio::process::Command::spawn`
/// fails (typically missing binary or permission denied).
pub async fn spawn(token: String) -> Result<Child> {
    Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "run", "--token", &token])
        .spawn()
        .map_err(|e| CoreError::Other(format!("spawning cloudflared: {e}")))
}

/// Supervises the `cloudflared` subprocess.
///
/// Loops: spawn, wait, restart with exponential backoff. The restart
/// budget is `MAX_RESTARTS` attempts inside `RESTART_WINDOW`; once
/// exceeded the supervisor logs and returns, leaving the tunnel down
/// until the operator restarts the agent.
///
/// The first spawn failure also returns immediately: if `cloudflared`
/// can't launch at all, retrying won't help.
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
