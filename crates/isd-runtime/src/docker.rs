//! Bollard wrapper. Constructs a `bollard::Docker` against either a
//! local socket or a tunneled remote socket exposed by [`SshTunnel`].

use crate::{Error, Result, SshTunnel};
use bollard::{API_DEFAULT_VERSION, Docker};
use std::collections::HashMap;

#[doc = include_str!("../docs/docker-backend.md")]
pub struct DockerBackend {
    /// `bollard::Docker` client. Talks to the daemon over whichever
    /// transport the constructor wired up.
    docker: Docker,
    /// Held to keep the ssh child alive while the backend is in use.
    /// `None` for local backends.
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
    /// Opens a backend against the local docker daemon.
    ///
    /// Uses `bollard::Docker::connect_with_local_defaults`: Unix socket
    /// on Linux, named pipe on Windows, `DOCKER_HOST` when the env var
    /// is set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when bollard cannot reach the local
    /// socket (daemon not running, permissions, missing `DOCKER_HOST`).
    pub fn from_local() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        Ok(Self {
            docker,
            _tunnel: None,
        })
    }

    /// Opens a backend that routes through an already-open SSH tunnel.
    ///
    /// Takes ownership of the tunnel so the ssh child stays alive for
    /// as long as the backend.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when bollard cannot connect to the
    /// tunneled HTTP endpoint (typically a closed tunnel or a port
    /// that the tunnel did not actually bind).
    pub fn from_tunnel(tunnel: SshTunnel) -> Result<Self> {
        let host = tunnel.docker_host();
        let docker = Docker::connect_with_http(&host, 4, API_DEFAULT_VERSION)?;
        Ok(Self {
            docker,
            _tunnel: Some(tunnel),
        })
    }

    /// Opens a backend from a docker endpoint URI.
    ///
    /// Supported schemes:
    ///
    /// - `ssh://user@host`: opens an [`SshTunnel`] and routes through it.
    /// - `unix:///var/run/docker.sock` or the literal string `local`:
    ///   uses the local socket.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidEndpoint`] for any other URI. Returns
    /// [`Error::SshTunnel`] when the ssh child fails to start or the
    /// forwarded port never becomes ready. Returns [`Error::Docker`]
    /// when bollard cannot connect to the chosen transport.
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

    /// Borrows the underlying bollard client.
    ///
    /// Use this to reach a daemon API the backend does not expose
    /// directly (image management, network ops, exec sessions). Most
    /// callers should prefer the higher-level methods on this struct.
    pub fn client(&self) -> &Docker {
        &self.docker
    }

    /// Liveness probe against the daemon.
    ///
    /// Calls `GET /version` and formats the result as
    /// `"<version> (<api_version>)"`. Doubles as a connectivity check
    /// for `isd doctor`-style flows.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when the daemon round-trip fails.
    pub async fn ping(&self) -> Result<String> {
        let v = self.docker.version().await?;
        Ok(format!(
            "{} ({})",
            v.version.unwrap_or_default(),
            v.api_version.unwrap_or_default()
        ))
    }

    /// Lists containers on the target daemon.
    ///
    /// `all = false` matches `docker ps` (running only); `all = true`
    /// matches `docker ps -a` (every container, including stopped).
    /// The returned rows are the CLI-facing [`ContainerSummary`] DTO,
    /// not the raw bollard struct.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] on daemon round-trip failure.
    pub async fn list_containers(&self, all: bool) -> Result<Vec<ContainerSummary>> {
        use bollard::container::ListContainersOptions;
        let opts = ListContainersOptions::<String> {
            all,
            ..Default::default()
        };
        let raw = self.docker.list_containers(Some(opts)).await?;
        Ok(raw.iter().map(map_summary).collect())
    }

    /// Stops a container by ID or name.
    ///
    /// `timeout_secs` is the grace period docker waits before sending
    /// `SIGKILL`. Bollard's default when `None` is 10 seconds; this
    /// method makes the value explicit at the boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when the daemon round-trip fails
    /// (container not found, already stopped, permission, etc.).
    pub async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let opts = bollard::container::StopContainerOptions { t: timeout_secs };
        self.docker.stop_container(id, Some(opts)).await?;
        Ok(())
    }

    /// Starts a stopped container.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when the daemon round-trip fails
    /// (container not found, already running, missing image, etc.).
    pub async fn start_container(&self, id: &str) -> Result<()> {
        self.docker.start_container::<String>(id, None).await?;
        Ok(())
    }

    /// Restarts a container.
    ///
    /// `timeout_secs` is the grace period before `SIGKILL`. Bollard
    /// 0.18 types this as `isize` on the restart options (versus `i64`
    /// on stop); this method keeps the public API consistent (`i64`)
    /// and casts at the boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] on daemon round-trip failure.
    pub async fn restart_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let opts = bollard::container::RestartContainerOptions {
            t: timeout_secs as isize,
        };
        self.docker.restart_container(id, Some(opts)).await?;
        Ok(())
    }

    /// Sends a signal to a running container.
    ///
    /// `signal` is a docker signal name (e.g. `"SIGHUP"`, `"SIGTERM"`).
    /// `None` defaults to `SIGKILL` (bollard's default).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when the daemon round-trip fails.
    pub async fn kill_container(&self, id: &str, signal: Option<&str>) -> Result<()> {
        let opts = signal.map(|s| bollard::container::KillContainerOptions { signal: s });
        self.docker.kill_container(id, opts).await?;
        Ok(())
    }

    /// Removes a container.
    ///
    /// `force = true` is the `docker rm -f` form: kills the container
    /// first if it is still running.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] when the daemon round-trip fails.
    pub async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
        let opts = bollard::container::RemoveContainerOptions {
            force,
            ..Default::default()
        };
        self.docker.remove_container(id, Some(opts)).await?;
        Ok(())
    }

    /// Fetches a container's labels by ID or name.
    ///
    /// Used by the protection guard (`isd rm`, `isd stop`,
    /// `isd restart`, `isd kill`) to detect
    /// `io.isengard.role=controller|agent` on a resolved target
    /// without listing every container on the host. Returns an empty
    /// map when the daemon omits labels; a label-less container is,
    /// by definition, not protected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Docker`] on daemon round-trip failure.
    pub async fn inspect_labels(&self, id: &str) -> Result<HashMap<String, String>> {
        let info = self.docker.inspect_container(id, None).await?;
        Ok(info.config.and_then(|c| c.labels).unwrap_or_default())
    }
}

/// A container row as the `isd` CLI consumes it.
///
/// `ContainerSummary` flattens bollard's raw container struct into
/// display-ready strings plus a few structured fields the interactive
/// commands (route wizard, protection guard) need. Built by
/// [`DockerBackend::list_containers`].
#[derive(Debug, Clone, Default)]
pub struct ContainerSummary {
    /// Full container ID. The CLI truncates this for display.
    pub id: String,
    /// Image reference, e.g. `nginx:1.27`.
    pub image: String,
    /// Docker's human status string, e.g. `Up 2 hours` or
    /// `Exited (0) 12 minutes ago`. Empty when the daemon omits it.
    pub status: String,
    /// Published and private ports, comma-joined, e.g.
    /// `0.0.0.0:8080->80/tcp, 5432/tcp`. Empty when none.
    /// Rendered with default IP and proto noise stripped (private
    /// helper `format_ports`).
    pub ports: String,
    /// Distinct private (container-internal) ports the container
    /// declares.
    ///
    /// Used by the interactive `isd route create` wizard to pre-fill
    /// the upstream port when there is exactly one obvious candidate.
    /// Order matches the daemon's enumeration; duplicates from
    /// IPv4 + IPv6 bindings are collapsed.
    pub private_ports: Vec<u16>,
    /// First container name with the leading `/` stripped.
    pub names: String,
    /// Container labels, forwarded verbatim from bollard.
    ///
    /// Used by the operator-side protection guard to detect
    /// `io.isengard.role` values. Empty when the daemon omits labels.
    pub labels: HashMap<String, String>,
}

/// Formats a bollard `Port` list the way `docker ps` renders the
/// `PORTS` column.
///
/// `ip:public->private/proto` for published ports,
/// `private/proto` for unpublished, comma-joined. Strips the boring
/// defaults (see [`format_one_port`]).
///
/// Dedupes IPv4 and IPv6 bindings that collapse to the same display
/// string after default-IP suppression. bollard returns
/// `0.0.0.0` (v4) and `[::]` (v6) as separate `Port` entries; without
/// dedupe every published port would render twice.
fn format_ports(ports: &[bollard::models::Port]) -> String {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for p in ports {
        let s = format_one_port(p);
        if !s.is_empty() && seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out.join(", ")
}

/// Renders one `Port` entry.
///
/// Drops noise that is true for almost every row: `0.0.0.0:` and
/// `[::]:` (the "all interfaces" IPs) and `/tcp` (the default proto).
/// Non-default IPs and protocols still render explicitly so the
/// operator sees them.
fn format_one_port(p: &bollard::models::Port) -> String {
    let proto_raw = p
        .typ
        .map(|t| format!("{t:?}").to_lowercase())
        .unwrap_or_else(|| "tcp".to_string());
    let proto_suffix = if proto_raw == "tcp" {
        String::new()
    } else {
        format!("/{proto_raw}")
    };
    let ip_prefix = match p.ip.as_deref() {
        None | Some("") | Some("0.0.0.0") | Some("::") => String::new(),
        Some(ip) => format!("{ip}:"),
    };
    match p.public_port {
        Some(public) => format!("{ip_prefix}{public}->{}{proto_suffix}", p.private_port),
        None => format!("{}{proto_suffix}", p.private_port),
    }
}

/// Flattens a bollard `ContainerSummary` into the CLI-facing DTO.
///
/// Strips the leading `/` from the first container name, hands the
/// raw label map through, and runs the ports list through
/// [`format_ports`] for display and [`distinct_private_ports`] for
/// the route wizard.
fn map_summary(c: &bollard::models::ContainerSummary) -> ContainerSummary {
    let names = c
        .names
        .as_ref()
        .and_then(|n| n.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let private_ports = c
        .ports
        .as_deref()
        .map(distinct_private_ports)
        .unwrap_or_default();
    ContainerSummary {
        id: c.id.clone().unwrap_or_default(),
        image: c.image.clone().unwrap_or_default(),
        status: c.status.clone().unwrap_or_default(),
        ports: c.ports.as_deref().map(format_ports).unwrap_or_default(),
        private_ports,
        names,
        labels: c.labels.clone().unwrap_or_default(),
    }
}

/// Distinct container-internal ports, in daemon-reported order.
///
/// Used by `isd route create`'s wizard to detect an unambiguous
/// upstream port. bollard reports each `Port` once per binding (often
/// v4 plus v6), so the function dedupes on `private_port`.
fn distinct_private_ports(ports: &[bollard::models::Port]) -> Vec<u16> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in ports {
        if seen.insert(p.private_port) {
            out.push(p.private_port);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the port-formatter and the
    //! bollard-to-[`ContainerSummary`] mapper. The backend methods are
    //! thin bollard wraps, so we exercise the wiring directly and
    //! defer real-daemon coverage to the `#[ignore]` smoke test below.

    use super::*;
    use bollard::models::{Port, PortTypeEnum};

    /// `0.0.0.0` IP and `tcp` proto are the boring defaults; the
    /// formatter suppresses both to keep the PORTS column readable.
    #[test]
    fn format_ports_drops_default_ip_and_proto() {
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
        assert_eq!(format_ports(&ports), "8080->80, 5432");
    }

    /// Non-default IPs and protocols render explicitly.
    #[test]
    fn format_ports_keeps_non_default_ip_and_proto() {
        let ports = vec![
            Port {
                ip: Some("192.168.1.1".into()),
                private_port: 80,
                public_port: Some(8080),
                typ: Some(PortTypeEnum::TCP),
            },
            Port {
                ip: None,
                private_port: 53,
                public_port: None,
                typ: Some(PortTypeEnum::UDP),
            },
        ];
        assert_eq!(format_ports(&ports), "192.168.1.1:8080->80, 53/udp");
    }

    /// bollard ships IPv4 and IPv6 bindings as separate entries. After
    /// default-IP suppression both render the same string; dedupe.
    #[test]
    fn format_ports_dedupes_ipv4_and_ipv6_bindings() {
        let ports = vec![
            Port {
                ip: Some("0.0.0.0".into()),
                private_port: 6767,
                public_port: Some(6767),
                typ: Some(PortTypeEnum::TCP),
            },
            Port {
                ip: Some("::".into()),
                private_port: 6767,
                public_port: Some(6767),
                typ: Some(PortTypeEnum::TCP),
            },
        ];
        assert_eq!(format_ports(&ports), "6767->6767");
    }

    /// An empty port list renders as an empty string, not a stray
    /// comma or whitespace.
    #[test]
    fn format_ports_empty_is_blank() {
        assert_eq!(format_ports(&[]), "");
    }

    /// Round-trips stop and start against the local daemon. Ignored
    /// because it needs a real engine; run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn stop_then_start_round_trips() {
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

    /// `map_summary` takes the first name, strips the leading `/`,
    /// and forwards id/image/status verbatim.
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
