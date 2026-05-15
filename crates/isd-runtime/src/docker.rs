//! Bollard wrapper. Constructs a `bollard::Docker` against either a
//! local socket or a tunneled remote socket exposed by [`SshTunnel`].

use crate::{Error, Result, SshTunnel};
use bollard::{API_DEFAULT_VERSION, Docker};

/// Backend handle the rest of `isd` uses to talk to a Docker daemon.
/// Owns the optional [`SshTunnel`] so the connection's lifetime is
/// bounded by the backend's lifetime.
///
/// `bollard::Docker` does not implement `Debug`; we manually derive a
/// minimal one so the test harness can `expect_err` cleanly.
pub struct DockerBackend {
    docker: Docker,
    // Held to keep the ssh child alive while the backend is in use.
    // None for local backends.
    _tunnel: Option<SshTunnel>,
}

impl std::fmt::Debug for DockerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerBackend")
            .field("tunneled", &self._tunnel.is_some())
            .finish_non_exhaustive()
    }
}

impl DockerBackend {
    /// Construct a backend that talks to the local docker daemon via
    /// `bollard::Docker::connect_with_local_defaults` (Unix socket on
    /// Linux, named pipe on Windows, `DOCKER_HOST` env if set).
    pub fn from_local() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        Ok(Self {
            docker,
            _tunnel: None,
        })
    }

    /// Construct a backend that talks to a remote docker daemon via
    /// an already-opened SSH tunnel.
    pub fn from_tunnel(tunnel: SshTunnel) -> Result<Self> {
        let host = tunnel.docker_host();
        let docker = Docker::connect_with_http(&host, 4, API_DEFAULT_VERSION)?;
        Ok(Self {
            docker,
            _tunnel: Some(tunnel),
        })
    }

    /// Open a backend from a docker endpoint URI:
    ///   - `ssh://user@host` opens a tunnel and routes through it
    ///   - `unix:///var/run/docker.sock` or `local` uses the local socket
    ///   - anything else returns [`Error::InvalidEndpoint`]
    pub async fn from_uri(uri: &str) -> Result<Self> {
        if let Some(ssh_target) = uri.strip_prefix("ssh://") {
            let tunnel = SshTunnel::open(ssh_target).await?;
            Self::from_tunnel(tunnel)
        } else if uri == "local" || uri.starts_with("unix://") {
            Self::from_local()
        } else {
            Err(Error::InvalidEndpoint(format!(
                "expected ssh://<target>, unix://<path>, or local; got {uri:?}"
            )))
        }
    }

    /// Borrow the underlying bollard client.
    pub fn client(&self) -> &Docker {
        &self.docker
    }

    /// Liveness probe. Returns "version (api_version)".
    pub async fn ping(&self) -> Result<String> {
        let v = self.docker.version().await?;
        Ok(format!(
            "{} ({})",
            v.version.unwrap_or_default(),
            v.api_version.unwrap_or_default()
        ))
    }
}
