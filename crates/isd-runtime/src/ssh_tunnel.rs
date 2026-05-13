//! SSH tunnel lifecycle. Spawns `ssh -L <local-port>:<remote-socket>
//! <target>` so a local TCP port forwards to the remote dockerd's Unix
//! socket. The struct owns the child process; Drop kills it.
//!
//! Requires OpenSSH 7+ on PATH (modern dev machines). Forward syntax
//! `-L <port>:/var/run/docker.sock` routes local TCP to a remote UNIX
//! socket; supported since OpenSSH 6.7 (released 2014).

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::sleep;

use crate::{Error, Result};

const REMOTE_DOCKER_SOCK: &str = "/var/run/docker.sock";

/// How long to wait between spawning ssh and returning the tunnel.
/// ssh -L binds the local port asynchronously; callers connecting too
/// early would race the bind. 750ms covers typical SSH handshakes on
/// a LAN. Remote hosts over high-latency links need a connect-retry
/// layer above this.
const READINESS_DELAY: Duration = Duration::from_millis(750);

#[derive(Debug)]
pub struct SshTunnel {
    /// Owned child; Drop sends start_kill so the ssh process exits
    /// when the tunnel value is dropped.
    child: Option<Child>,
    local_port: u16,
}

impl SshTunnel {
    /// Acquire a free local TCP port by binding to 127.0.0.1:0,
    /// reading back the assigned port, then dropping the listener.
    /// The port may be reused between drop and `ssh -L` binding; that
    /// race is acceptable for a dev-CLI tunnel (ssh fails loudly with
    /// "Address already in use" and the caller retries).
    pub fn acquire_local_port() -> Result<u16> {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("static");
        let listener = TcpListener::bind(addr)?;
        Ok(listener.local_addr()?.port())
    }

    /// Open an ssh tunnel to `ssh_target`, forwarding a fresh local
    /// TCP port to the remote docker socket. Returned struct owns the
    /// ssh child; Drop kills it. `ssh_target` accepts any form OpenSSH
    /// accepts: `user@host`, an entry from `~/.ssh/config`, `host:port`.
    pub async fn open(ssh_target: &str) -> Result<Self> {
        let local_port = Self::acquire_local_port()?;
        let forward = format!("{local_port}:{REMOTE_DOCKER_SOCK}");
        let child = Command::new("ssh")
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-L")
            .arg(&forward)
            .arg(ssh_target)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::SshTunnel(format!("spawn ssh: {e}")))?;

        sleep(READINESS_DELAY).await;

        Ok(Self {
            child: Some(child),
            local_port,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// URL bollard accepts for connecting to the tunneled remote.
    pub fn docker_host(&self) -> String {
        format!("tcp://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // start_kill is fire-and-forget; the tokio runtime may
            // still be alive at Drop time so we cannot await the
            // child here. The kernel reaps the process.
            let _ = child.start_kill();
        }
    }
}
