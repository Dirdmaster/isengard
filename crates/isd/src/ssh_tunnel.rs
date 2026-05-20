//! Lifetime-managed SSH LocalForward to the controller's published REST
//! port.
//!
//! `Tunnel::open_local_forward` shells out to system `ssh` with
//! `-L <ephemeral>:<remote_host>:<remote_port>` and `-N` (no remote
//! command), piggybacking on the ControlMaster the isd-runtime docker
//! tunnel set up against the same target. The operator's `~/.ssh/config`
//! does the heavy lifting (Hosts, IdentityFile, ProxyJump, agent
//! forwarding); we pass the target verbatim. Modeled on
//! docker-context-ssh and podman-remote.
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

/// Live SSH port-forward. The remote `<host>:<port>` is reachable as
/// `127.0.0.1:<local_port>` on the operator's machine.
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
    /// Open a port-forward from a fresh local TCP port to
    /// `remote_host:remote_port` on the SSH `target`, piggybacking on the
    /// ControlMaster the isd-runtime docker tunnel set up against the
    /// same target. Used to forward the controller container's published
    /// REST port (typically `127.0.0.1:9418` on the remote) to a local
    /// loopback port reqwest can hit.
    ///
    /// The ControlPath is the same hash isd-runtime uses
    /// ([`isd_runtime::control_path_for`]) so multiple LocalForwards
    /// against the same SSH target share one underlying SSH connection.
    /// ControlPersist on the master keeps the connection warm for
    /// subsequent invocations.
    pub async fn open_local_forward(
        target: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self> {
        let local_port = pick_ephemeral_port().await?;
        let forward_arg = format!("{local_port}:{remote_host}:{remote_port}");
        let control_path = isd_runtime::control_path_for(target);

        let mut child = Command::new("ssh")
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPath={control_path}"))
            .arg("-o")
            .arg("ControlPersist=10m")
            .arg("-L")
            .arg(&forward_arg)
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning `ssh` for LocalForward (is it installed and on $PATH?)")?;

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

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                // When an existing ControlMaster socket is reachable
                // (the common case after `isd init` or any prior
                // `isd` command set one up), ssh dispatches the
                // forward request to the master and exits cleanly.
                // The master owns the forward; the local port is now
                // listening even though our spawned client is gone.
                // Treat exit-0-with-listening-port as success.
                let listening = tokio::net::TcpStream::connect(("127.0.0.1", local_port))
                    .await
                    .is_ok();
                if status.success() && listening {
                    return Ok(Tunnel {
                        child,
                        local_port,
                        stderr_buf,
                    });
                }
                let stderr = stderr_buf.lock().await.clone();
                anyhow::bail!(
                    "ssh LocalForward to {target:?} (-> {remote_host}:{remote_port}) failed (exit {status}): {}",
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
                    "ssh LocalForward to {target:?} (-> {remote_host}:{remote_port}) did not come up within 5s. Last stderr: {}",
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
}
