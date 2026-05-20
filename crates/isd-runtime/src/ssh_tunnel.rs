#![doc = include_str!("../docs/ssh-tunnel.md")]

use std::net::{SocketAddr, TcpListener};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

use crate::{Error, Result};

/// Remote path the tunnel forwards to. Docker's well-known Unix socket
/// on every platform that ships dockerd.
const REMOTE_DOCKER_SOCK: &str = "/var/run/docker.sock";

/// Upper bound on how long the readiness probe polls the local port
/// before giving up.
///
/// The poll exits the moment a TCP connect succeeds, so on a fast LAN
/// the function returns in tens of milliseconds; this cap only matters
/// for slow links or a misconfigured target.
const READINESS_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between TCP-connect probes while waiting for the forwarded
/// port to come up.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long ssh keeps an idle multiplex master alive after the last
/// client exits.
///
/// Long enough that an operator running `isd ps` then `isd stop 0`
/// reuses the connection. Short enough that a forgotten session does
/// not linger forever.
const CONTROL_PERSIST: &str = "10m";

/// SSH tunnel that forwards a local TCP port to a remote docker socket.
///
/// See the module docs for the trust model and multiplexing behaviour.
/// Construct via [`SshTunnel::open`]; the returned value owns the ssh
/// child and kills it on `Drop`.
#[derive(Debug)]
pub struct SshTunnel {
    /// Owned ssh child. `Drop` calls `start_kill` so the process
    /// exits when the tunnel value is dropped. `Option` so `Drop` can
    /// take ownership without `unsafe`.
    child: Option<Child>,
    /// Local TCP port bound by `ssh -L`. The bollard client targets
    /// `tcp://127.0.0.1:<this>`.
    local_port: u16,
}

impl SshTunnel {
    /// Acquires a free local TCP port from the kernel.
    ///
    /// Binds to `127.0.0.1:0`, reads back the assigned port, then drops
    /// the listener. The port may be reused between drop and the
    /// subsequent `ssh -L` binding; that race is acceptable for a
    /// dev-CLI tunnel because ssh fails loudly with
    /// `Address already in use` and the caller retries.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the kernel cannot hand out a port
    /// (e.g. ulimit on open sockets exhausted).
    pub fn acquire_local_port() -> Result<u16> {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("static");
        let listener = TcpListener::bind(addr)?;
        Ok(listener.local_addr()?.port())
    }

    /// Opens an ssh tunnel to `ssh_target` and waits for the forwarded
    /// port to become ready.
    ///
    /// `ssh_target` accepts any form OpenSSH accepts: `user@host`, an
    /// entry from `~/.ssh/config`, `host:port`. The function spawns
    /// `ssh -N -T -L <local>:/var/run/docker.sock <target>` with
    /// `ControlMaster=auto`. The returned tunnel owns the ssh child.
    ///
    /// Subsequent calls for the same `ssh_target` within the
    /// `ControlPersist` window reuse the existing master socket and
    /// return as soon as the new forward becomes ready.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SshTunnel`] when the ssh child fails to spawn
    /// or when the forwarded port does not become ready inside the
    /// readiness timeout (5 seconds). Returns [`Error::Io`] when port
    /// acquisition fails.
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
            // Silence the remote MOTD, login banner, PAM session
            // messages. ssh's stdout inherits ours by default, so any
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

    /// Returns the kernel-assigned local port the tunnel is bound to.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Returns the URL bollard accepts for the tunneled remote daemon.
    ///
    /// Format: `tcp://127.0.0.1:<local_port>`. Hand this to
    /// `bollard::Docker::connect_with_http`.
    pub fn docker_host(&self) -> String {
        format!("tcp://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // `start_kill` is fire-and-forget. The tokio runtime may
            // still be alive at `Drop` time so we cannot await the
            // child here. The kernel reaps the process. The master
            // ssh connection survives via `ControlPersist` so the next
            // `open()` against the same target can reuse it.
            let _ = child.start_kill();
        }
    }
}

/// Polls the local port until a TCP connect succeeds or the timeout
/// elapses.
///
/// Returns the moment the first connect succeeds (typically tens of
/// milliseconds once the multiplexed master is warm).
///
/// # Errors
///
/// Returns [`Error::SshTunnel`] when the port has not become reachable
/// after [`READINESS_TIMEOUT`].
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

/// Builds the per-target ControlPath under the user's temp dir.
///
/// The path's uniqueness comes from a stable hash of the target string
/// so a second `isd` invocation against the same target finds the
/// existing master socket. The function avoids ssh's `%r@%h:%p` tokens
/// because the operator may pass any of `user@host`, a bare alias
/// from `~/.ssh/config`, or `host:port`; those tokens expand
/// inconsistently across those forms.
///
/// The `isd` controller-REST tunnel reuses this exact hash so the
/// REST `LocalForward` piggybacks on the docker-socket forward's
/// `ControlMaster` instead of opening a second TCP handshake to the
/// same host.
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
