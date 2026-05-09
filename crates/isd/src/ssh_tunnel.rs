//! Lifetime-managed SSH port-forward to a remote controller's dashboard.
//!
//! `Tunnel::open` shells out to system `ssh` with `-L <ephemeral>:127.0.0.1:<port>`
//! and `-N` (no remote command). The operator's `~/.ssh/config` does the
//! heavy lifting (Hosts, IdentityFile, ProxyJump, agent forwarding); we
//! pass the target verbatim. Modeled on docker-context-ssh and
//! podman-remote.
//!
//! Lifecycle: the `Tunnel` value owns the ssh child process. `Drop` kills
//! the child via `start_kill` (SIGKILL on Unix). For long-running
//! commands (e.g. `isd logs -f`), the tunnel lives until isd exits.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

/// Live SSH port-forward. The remote `127.0.0.1:<dashboard_port>` is
/// reachable as `127.0.0.1:<local_port>` on the operator's machine.
pub struct Tunnel {
    /// Spawned `ssh -N -L ... <target>` process.
    child: Child,
    /// Loopback port we bind on the local side. Drawn from the kernel via
    /// a transient `bind(0)`; small (microsecond) race window before ssh
    /// actually grabs it but in practice fine.
    pub local_port: u16,
    /// Captured stderr of the ssh process; surfaced into errors when the
    /// tunnel fails to come up so the operator sees `ssh: connect to host
    /// X: connection refused` etc instead of a generic timeout. Kept on
    /// the struct so the background reader task's Arc-clone stays live.
    #[allow(dead_code)]
    stderr_buf: std::sync::Arc<tokio::sync::Mutex<String>>,
}

impl Tunnel {
    /// Open a port-forward to `target` (anything `ssh` understands:
    /// `user@host`, `host`, or a `Host` alias) forwarding the remote
    /// `127.0.0.1:<dashboard_port>` to a local ephemeral port. Waits up
    /// to 5s for the forward to come up.
    pub async fn open(target: &str, dashboard_port: u16) -> Result<Self> {
        let local_port = pick_ephemeral_port().await?;
        let forward_arg = format!("{local_port}:127.0.0.1:{dashboard_port}");

        // -N: don't run a remote command; we just want the forward.
        // -T: no pty (saves resources, fewer warnings on quiet hosts).
        // ExitOnForwardFailure=yes: ssh exits if the forward can't be set
        //   up at the destination (e.g., remote refuses, remote port not
        //   listening). Without this we'd hang waiting for a connection
        //   that will never come.
        // ServerAliveInterval=15: detect dead sessions for long-running
        //   commands like `isd logs -f`.
        // BatchMode=yes: never prompt for a password. SSH key or
        //   ssh-agent is required; otherwise fail fast.
        let mut child = Command::new("ssh")
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-L")
            .arg(&forward_arg)
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning `ssh` (is it installed and on $PATH?)")?;

        let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let buf = std::sync::Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut guard = buf.lock().await;
                    guard.push_str(&line);
                    guard.push('\n');
                }
            });
        }

        // Poll the local port until it answers. ssh sets up the listener
        // before the remote tunnel is fully wired, so a successful TCP
        // connect here means the forward exists.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            // Detect early ssh failure (auth refused, host unreachable):
            // child has already exited.
            if let Ok(Some(status)) = child.try_wait() {
                let stderr = stderr_buf.lock().await.clone();
                anyhow::bail!(
                    "ssh tunnel to {target:?} failed (exit {status}): {}",
                    stderr.trim().lines().last().unwrap_or("(no stderr)")
                );
            }

            if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
                .await
                .is_ok()
            {
                return Ok(Tunnel {
                    child,
                    local_port,
                    stderr_buf,
                });
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                let stderr = stderr_buf.lock().await.clone();
                anyhow::bail!(
                    "ssh tunnel to {target:?} did not come up within 5s. Last stderr: {}",
                    stderr.trim().lines().last().unwrap_or("(none)")
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// HTTP base URL for the locally-forwarded port. Subcommands can
    /// build REST URLs by appending `/api/v1/...`.
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // `kill_on_drop(true)` was set on spawn so tokio sends SIGKILL
        // automatically when the Child is dropped. This explicit
        // `start_kill` is belt + suspenders for the case where the child
        // somehow isn't owned anymore.
        let _ = self.child.start_kill();
    }
}

/// Bind a TCP listener to port 0 to get an OS-assigned ephemeral port,
/// then immediately drop the listener so ssh can claim the port. There
/// is a microsecond race window where another process could grab the
/// port between drop and ssh's `listen()`; in practice we've never
/// observed it.
async fn pick_ephemeral_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding ephemeral port for SSH forward")?;
    let port = listener.local_addr().context("reading bound port")?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pick_ephemeral_port_returns_nonzero_unprivileged_port() {
        let port = pick_ephemeral_port().await.unwrap();
        assert!(port >= 1024, "got privileged port {port}");
    }

    #[tokio::test]
    async fn open_unreachable_target_fails_fast() {
        // Pick a target that won't resolve / accept. `localhost:1` has
        // nothing listening; ssh's BatchMode + ExitOnForwardFailure
        // should make this fail in well under our 5s budget.
        let res = Tunnel::open("nonexistent.invalid", 9418).await;
        assert!(res.is_err(), "expected ssh tunnel failure, got Ok");
    }
}
