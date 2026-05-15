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

    /// List containers on the target daemon. `all = false` matches
    /// `docker ps` (running only); `all = true` matches `docker ps -a`.
    pub async fn list_containers(&self, all: bool) -> Result<Vec<ContainerSummary>> {
        use bollard::container::ListContainersOptions;
        let opts = ListContainersOptions::<String> {
            all,
            ..Default::default()
        };
        let raw = self.docker.list_containers(Some(opts)).await?;
        Ok(raw.iter().map(map_summary).collect())
    }

    /// Stop a container by ID or name. `timeout_secs` is the grace period
    /// before docker sends SIGKILL; bollard's default when None is 10s.
    pub async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let opts = bollard::container::StopContainerOptions { t: timeout_secs };
        self.docker.stop_container(id, Some(opts)).await?;
        Ok(())
    }

    /// Start a stopped container.
    pub async fn start_container(&self, id: &str) -> Result<()> {
        self.docker.start_container::<String>(id, None).await?;
        Ok(())
    }

    /// Restart a container. `timeout_secs` is the grace period before
    /// SIGKILL. Note: bollard 0.18 types this as `isize` on the
    /// restart options (vs `i64` on stop); we keep the public API
    /// consistent (`i64`) and cast at the boundary.
    pub async fn restart_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let opts = bollard::container::RestartContainerOptions {
            t: timeout_secs as isize,
        };
        self.docker.restart_container(id, Some(opts)).await?;
        Ok(())
    }

    /// Send a signal to a running container. `signal` defaults to
    /// SIGKILL when None.
    pub async fn kill_container(&self, id: &str, signal: Option<&str>) -> Result<()> {
        let opts = signal.map(|s| bollard::container::KillContainerOptions { signal: s });
        self.docker.kill_container(id, opts).await?;
        Ok(())
    }

    /// Remove a container. `force = true` is the `docker rm -f` form
    /// (kills if running).
    pub async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
        let opts = bollard::container::RemoveContainerOptions {
            force,
            ..Default::default()
        };
        self.docker.remove_container(id, Some(opts)).await?;
        Ok(())
    }
}

/// A container row as the `isd` CLI consumes it: bollard's
/// `ContainerSummary` flattened to display-ready strings. Built by
/// [`DockerBackend::list_containers`].
#[derive(Debug, Clone)]
pub struct ContainerSummary {
    /// Full container ID. The CLI truncates for display.
    pub id: String,
    /// Image ref, e.g. `nginx:1.27`.
    pub image: String,
    /// Docker's human status string, e.g. `Up 2 hours`,
    /// `Exited (0) 12 minutes ago`. Empty when the daemon omits it.
    pub status: String,
    /// Published + private ports, comma-joined, e.g.
    /// `0.0.0.0:8080->80/tcp, 5432/tcp`. Empty when none.
    pub ports: String,
    /// First container name, leading `/` stripped.
    pub names: String,
}

/// Format a bollard `Port` list the way `docker ps` renders the PORTS
/// column: `ip:public->private/proto` for published ports,
/// `private/proto` for unpublished. Comma-joined.
fn format_ports(ports: &[bollard::models::Port]) -> String {
    ports
        .iter()
        .map(|p| {
            let proto = p
                .typ
                .map(|t| format!("{t:?}").to_lowercase())
                .unwrap_or_else(|| "tcp".to_string());
            match (p.ip.as_deref(), p.public_port) {
                (Some(ip), Some(public)) => {
                    format!("{ip}:{public}->{}/{proto}", p.private_port)
                }
                (None, Some(public)) => {
                    format!("{public}->{}/{proto}", p.private_port)
                }
                _ => format!("{}/{proto}", p.private_port),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Flatten a bollard `ContainerSummary` into the CLI-facing DTO.
fn map_summary(c: &bollard::models::ContainerSummary) -> ContainerSummary {
    let names = c
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_default();
    ContainerSummary {
        id: c.id.clone().unwrap_or_default(),
        image: c.image.clone().unwrap_or_default(),
        status: c.status.clone().unwrap_or_default(),
        ports: c.ports.as_deref().map(format_ports).unwrap_or_default(),
        names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{Port, PortTypeEnum};

    #[test]
    fn format_ports_renders_published_and_private() {
        let ports = vec![
            Port {
                ip: Some("0.0.0.0".into()),
                private_port: 80,
                public_port: Some(8080),
                typ: Some(PortTypeEnum::TCP),
            },
            Port {
                ip: None,
                private_port: 5432,
                public_port: None,
                typ: Some(PortTypeEnum::TCP),
            },
        ];
        let rendered = format_ports(&ports);
        assert_eq!(rendered, "0.0.0.0:8080->80/tcp, 5432/tcp");
    }

    #[test]
    fn format_ports_empty_is_blank() {
        assert_eq!(format_ports(&[]), "");
    }

    // Pure unit tests: every backend method is a thin bollard wrap, so
    // we test the wiring (method exists, takes the right shape) and let
    // the ignored smoke test below cover the real daemon round-trip.

    #[tokio::test]
    #[ignore]
    async fn stop_then_start_round_trips() {
        // Smoke against the local docker daemon. Spins up a sleeping
        // container, stops it, starts it, removes it. Ignored because
        // it needs a real daemon; run with --ignored.
        let backend = DockerBackend::from_local().expect("local backend");
        let body = bollard::container::Config {
            image: Some("alpine:latest".to_string()),
            cmd: Some(vec!["sleep".into(), "3600".into()]),
            ..Default::default()
        };
        let id = backend
            .client()
            .create_container::<&str, _>(None, body)
            .await
            .expect("create")
            .id;
        backend.start_container(&id).await.expect("start");
        backend.stop_container(&id, 5).await.expect("stop");
        backend
            .start_container(&id)
            .await
            .expect("restart-via-start");
        backend.remove_container(&id, true).await.expect("rm -f");
    }

    #[test]
    fn map_summary_strips_leading_slash_and_takes_first_name() {
        let raw = bollard::models::ContainerSummary {
            id: Some("a1b2c3d4e5f6deadbeef".into()),
            names: Some(vec!["/web-proxy".into(), "/web-proxy-alias".into()]),
            image: Some("nginx:1.27".into()),
            status: Some("Up 2 hours".into()),
            ports: Some(vec![]),
            ..Default::default()
        };
        let s = map_summary(&raw);
        assert_eq!(s.id, "a1b2c3d4e5f6deadbeef");
        assert_eq!(s.image, "nginx:1.27");
        assert_eq!(s.status, "Up 2 hours");
        assert_eq!(s.names, "web-proxy");
    }
}
