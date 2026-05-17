//! SSH tunnel lifecycle. Spawns `ssh -L <local-port>:<remote-socket>
//! <target>` so a local TCP port forwards to the remote dockerd's Unix
//! socket. The struct owns the child process; Drop kills it.
//!
//! Requires OpenSSH 7+ on PATH (modern dev machines). Forward syntax
//! `-L <port>:/var/run/docker.sock` routes local TCP to a remote UNIX
//! socket; supported since OpenSSH 6.7 (released 2014).
//!
//! Uses ssh ControlMaster multiplexing so back-to-back invocations
//! (e.g. `isd ps; isd ps`) reuse one ssh connection instead of paying
//! the handshake cost every call. First call: ~1-2s (handshake).
//! Subsequent calls within ControlPersist window: ~100ms.

use std::net::{SocketAddr, TcpListener};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

use crate::{Error, Result};

const REMOTE_DOCKER_SOCK: &str = "/var/run/docker.sock";

/// Upper bound on how long we poll the local port for tunnel readiness
/// before giving up. The poll exits the moment a TCP connect succeeds,
/// so on a fast LAN we return in tens of ms; this cap only matters for
/// slow links or a misconfigured target.
const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long ssh keeps an idle multiplex master alive after the last
/// client exits. Long enough that an operator running `isd ps` then
/// `isd stop 0` reuses the connection; short enough that a forgotten
/// session does not linger forever.
const CONTROL_PERSIST: &str = "10m";

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
    ///
    /// Uses ControlMaster multiplexing per `ssh_target`: subsequent
    /// `open()` calls for the same target within the ControlPersist
    /// window reuse one ssh connection.
    pub async fn open(ssh_target: &str) -> Result<Self> {
        let local_port = Self::acquire_local_port()?;
        let forward = format!("{local_port}:{REMOTE_DOCKER_SOCK}");
        let control_path = control_path_for(ssh_target);
        let child = Command::new("ssh")
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPath={control_path}"))
            .arg("-o")
            .arg(format!("ControlPersist={CONTROL_PERSIST}"))
            .arg("-L")
            .arg(&forward)
            .arg(ssh_target)
            // Silence the remote MOTD / login banner / PAM session
            // messages: ssh's stdout inherits ours by default, so any
            // bytes it writes land in `isd ps`'s table output.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::SshTunnel(format!("spawn ssh: {e}")))?;

        wait_for_port_ready(local_port).await?;

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
            // child here. The kernel reaps the process. The master
            // ssh connection survives via ControlPersist so the next
            // open() call against the same target can reuse it.
            let _ = child.start_kill();
        }
    }
}

/// Poll the local port until a TCP connect succeeds or the timeout
/// elapses. Returns immediately on the first success (typically tens
/// of ms once the multiplexed master is warm).
async fn wait_for_port_ready(port: u16) -> Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("static");
    let start = Instant::now();
    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if start.elapsed() >= READINESS_TIMEOUT {
            return Err(Error::SshTunnel(format!(
                "ssh tunnel on 127.0.0.1:{port} did not become ready within {:?}",
                READINESS_TIMEOUT
            )));
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

/// Per-target ControlPath under the user's temp dir. The path's
/// uniqueness comes from a stable hash of the target string so a
/// second `isd` invocation against the same target finds the existing
/// master socket. We avoid `%r@%h:%p` ssh tokens because the operator
/// may pass any of `user@host`, a bare alias from ~/.ssh/config, or
/// `host:port`; the tokens expand inconsistently across those forms.
///
/// Exposed pub(crate)-style via the [`control_path_for`] helper module
/// re-export: Phase 4 (`isd::ssh_tunnel::Tunnel::open_local_forward`)
/// reuses the exact same hash so the controller-REST LocalForward
/// piggybacks on the docker-socket forward's ControlMaster instead of
/// opening a second TCP handshake to the same host.
pub fn control_path_for(ssh_target: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    ssh_target.hash(&mut h);
    let hex = format!("{:x}", h.finish());
    let dir = std::env::temp_dir();
    dir.join(format!("isd-ssh-{hex}.sock"))
        .to_string_lossy()
        .into_owned()
}
