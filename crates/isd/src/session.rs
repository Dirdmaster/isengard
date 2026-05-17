//! Per-command session: resolves a [`ContextEntry`] into a usable
//! `reqwest::Client` + base URL + (when SSH-backed) live tunnel.
//!
//! Replaces the old `login::pinned_session` which assumed every context
//! was a directly-reachable URL. The session owns the tunnel for its
//! lifetime; dropping the session closes the tunnel.
//!
//! Subcommands now take `&Session` instead of `(&ContextEntry,
//! &reqwest::Client)`. The base URL is `session.require_controller()?`;
//! for HTTP backends that's the saved URL, for SSH backends it's the
//! local loopback port the tunnel forwards to. Container verbs that
//! route through `DockerBackend` directly should not call
//! `require_controller`: they don't need the controller and the call
//! returns the actionable `isd init` error when no controller is found.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};

use crate::context::NO_CONTROLLER_SENTINEL;
use crate::credentials::{self, Backend, ContextEntry};
use crate::ssh_tunnel::Tunnel;

/// Resolved context plus a usable HTTP client. Hold the value for as long
/// as you need to talk to the controller; dropping it tears the SSH
/// tunnel down.
pub struct Session {
    /// The resolved context backing this session. Surfaces in error
    /// messages and (future) richer `isd context show <name>` output.
    #[allow(dead_code)]
    pub context: ContextEntry,
    pub client: reqwest::Client,
    /// Base URL for REST calls. For HTTP backends, equals the saved URL.
    /// For SSH backends, equals `http://127.0.0.1:<tunnel.local_port>`.
    /// For Docker backends with a discovered controller, equals the
    /// LocalForward / direct URL. `None` when the Docker context has no
    /// controller on the host: container verbs still work via the
    /// docker_uri side-channel; orchestration verbs call
    /// `require_controller` and surface the actionable `isd init` error.
    controller_url: Option<String>,
    /// SSH tunnel held alive for the session. `None` for HTTP backends.
    /// Field is read via `Drop` only; allow_dead_code keeps clippy quiet.
    #[allow(dead_code)]
    tunnel: Option<Tunnel>,
    /// `Some` when discovery succeeded against a Docker context. Always
    /// `None` for Http / Ssh backends (those imply a deliberately
    /// configured controller, so missing-controller is an error).
    #[allow(dead_code)]
    discovered_endpoint: Option<isd_runtime::ControllerEndpoint>,
}

impl Session {
    /// Open a session for the named context (or the file's default if
    /// `name` is None). Loads the credentials file, resolves the
    /// backend, and (for SSH) opens a port-forward.
    pub async fn open(name: Option<&str>) -> Result<Self> {
        let path = credentials::default_credentials_path()?;
        let file = credentials::load(&path)?;
        let ctx = file.default_or_named(name)?.clone();
        Self::from_context(ctx).await
    }

    /// Open a session against a specific context entry. Useful for tests
    /// and for `isd context show` which inspects without dispatching to
    /// the wire.
    pub async fn from_context(ctx: ContextEntry) -> Result<Self> {
        // Docker-only contexts park the NO_CONTROLLER_SENTINEL on the
        // HTTP variant. Catching it here keeps the sentinel off the
        // wire so controller-bound verbs surface a useful message
        // instead of a DNS lookup failure on `no-controller.invalid`.
        if let Backend::Http { url } = &ctx.backend
            && url == NO_CONTROLLER_SENTINEL
        {
            return Err(anyhow!(
                "context {:?} is docker-only and has no controller; this command needs one. \
                 Either add a controller backend with `isd context create {} --ssh <target>` \
                 (or `--http <url>`), or run against a different context via `--context <name>`.",
                ctx.name,
                ctx.name,
            ));
        }
        let (controller_url, tunnel, discovered_endpoint) = match &ctx.backend {
            Backend::Http { url } => (Some(url.trim_end_matches('/').to_string()), None, None),
            Backend::Ssh {
                target,
                dashboard_port,
            } => {
                let tunnel = Tunnel::open(target, *dashboard_port)
                    .await
                    .with_context(|| {
                        format!("opening SSH tunnel to {target} (context {:?})", ctx.name)
                    })?;
                (Some(tunnel.local_url()), Some(tunnel), None)
            }
            // Track D Phase 4: connect to docker via the configured URL,
            // discover the controller container by label, and (for SSH-
            // backed docker URLs) open a second LocalForward via the
            // existing ControlMaster so reqwest can hit the controller's
            // published REST port over loopback.
            //
            // Track E: when discovery returns NotFound, fall through to
            // a controller-less session so container verbs still work.
            // Other discovery failures (Multiple, version skew, missing
            // labels, docker API errors) still propagate as hard errors:
            // those indicate a misconfigured host, not a fresh one.
            Backend::Docker { url } => {
                let backend = isd_runtime::DockerBackend::from_uri(url)
                    .await
                    .with_context(|| {
                        format!("connecting to docker at {url} for context {:?}", ctx.name)
                    })?;
                match isd_runtime::discover(backend.client()).await {
                    Ok(endpoint) => {
                        let (controller_url, tunnel) = if let Some(target) =
                            url.strip_prefix("ssh://")
                        {
                            // ssh://[user@]host[:port][/path] -> [user@]host[:port].
                            // docker's URL form does not carry a path, but strip
                            // defensively in case an operator pasted one.
                            let target = target.split('/').next().unwrap_or(target);
                            let tunnel = Tunnel::open_local_forward(
                                target,
                                &endpoint.host_ip,
                                endpoint.host_port,
                            )
                            .await
                            .with_context(|| {
                                format!(
                                    "opening SSH LocalForward to controller \
                                     {}:{} via {target} (context {:?})",
                                    endpoint.host_ip, endpoint.host_port, ctx.name
                                )
                            })?;
                            (tunnel.local_url(), Some(tunnel))
                        } else {
                            // tcp:// or unix:// docker context: the discovered
                            // host:port is reachable from this machine directly
                            // (the operator's docker context already worked).
                            // Caveat: the compose recipe binds 127.0.0.1:9418 by
                            // default, so a unix-socket docker context only
                            // works when this machine *is* the controller host.
                            (
                                format!("http://{}:{}", endpoint.host_ip, endpoint.host_port),
                                None,
                            )
                        };
                        (Some(controller_url), tunnel, Some(endpoint))
                    }
                    Err(isd_runtime::DiscoveryError::NotFound) => {
                        // Track E: container verbs work without a controller.
                        // Orchestration verbs call session.require_controller()
                        // and receive the actionable "run isd init" message
                        // there.
                        (None, None, None)
                    }
                    Err(other) => {
                        return Err(anyhow::Error::from(other).context(format!(
                            "discovering isengard controller via docker at {url}",
                        )));
                    }
                }
            }
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;

        Ok(Session {
            context: ctx,
            client,
            controller_url,
            tunnel,
            discovered_endpoint,
        })
    }

    /// Return the controller URL or an actionable error pointing at
    /// `isd init`. Use this in orchestration verbs that require a
    /// controller. Container verbs that route through `DockerBackend`
    /// directly should NOT call this: they don't need the controller.
    pub fn require_controller(&self) -> Result<&str> {
        self.controller_url.as_deref().ok_or_else(|| {
            anyhow!(
                "no isengard controller on this host. \
                 Run `isd init` to bootstrap one, or point at a different \
                 context via --context <name>."
            )
        })
    }

    /// Return the controller URL if discovery succeeded, else `None`.
    /// Allows callers that want to gracefully degrade without erroring.
    #[allow(dead_code)]
    pub fn controller_url_opt(&self) -> Option<&str> {
        self.controller_url.as_deref()
    }

    /// Apply per-context auth to a request builder. Both backends are
    /// auth-less today: SSH-backed contexts rely on the operator's SSH
    /// credential, HTTP-backed contexts predate any dashboard middleware.
    /// Reserved as a seam for when we add real auth without touching
    /// every call site. Currently a no-op.
    #[allow(dead_code)]
    pub fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn docker_backend_session_open_yields_no_controller_or_docker_error() {
        // Track E: when discovery returns NotFound we now produce a
        // controller-less Session instead of erroring (container verbs
        // still work via DockerBackend; orchestration verbs call
        // require_controller). When the docker socket itself is
        // unreachable, discover returns DiscoveryError::Docker which we
        // still propagate as a hard error. Both outcomes are acceptable
        // here: this machine may or may not have a local docker daemon.
        let ctx = ContextEntry {
            name: "lausanne".into(),
            backend: Backend::Docker {
                url: "unix:///nonexistent/docker.sock".into(),
            },
            docker: None,
        };
        match Session::from_context(ctx).await {
            Ok(session) => {
                // Docker reachable, no controller container: orchestration
                // verbs should see the actionable error from require_controller.
                assert!(
                    session.controller_url_opt().is_none(),
                    "no controller container should mean no controller_url"
                );
                let err = session.require_controller().unwrap_err();
                let msg = format!("{err}");
                assert!(msg.contains("no isengard controller"), "msg: {msg}");
                assert!(msg.contains("isd init"), "msg: {msg}");
            }
            Err(e) => {
                // Docker unreachable: the error chain mentions docker so
                // the operator sees a useful breadcrumb.
                let msg = format!("{e:#}");
                assert!(msg.contains("docker"), "msg: {msg}");
            }
        }
    }

    #[tokio::test]
    async fn require_controller_returns_actionable_error_when_unset() {
        // Build a Session by hand so the test doesn't need a live docker
        // socket. The shape exercises the require_controller helper
        // directly: when controller_url is None, the error string must
        // mention both `no isengard controller` and `isd init`.
        let session = Session {
            context: ContextEntry {
                name: "lausanne".into(),
                backend: Backend::Docker {
                    url: "ssh://op@host".into(),
                },
                docker: None,
            },
            client: reqwest::Client::new(),
            controller_url: None,
            tunnel: None,
            discovered_endpoint: None,
        };
        let err = session
            .require_controller()
            .expect_err("controller_url is None; require_controller should error");
        let msg = format!("{err}");
        assert!(msg.contains("no isengard controller"), "msg: {msg}");
        assert!(msg.contains("isd init"), "msg: {msg}");
    }

    #[tokio::test]
    async fn require_controller_returns_url_when_set() {
        let session = Session {
            context: ContextEntry {
                name: "lausanne".into(),
                backend: Backend::Http {
                    url: "https://controller.example".into(),
                },
                docker: None,
            },
            client: reqwest::Client::new(),
            controller_url: Some("https://controller.example".into()),
            tunnel: None,
            discovered_endpoint: None,
        };
        assert_eq!(
            session.require_controller().unwrap(),
            "https://controller.example"
        );
    }

    #[tokio::test]
    async fn docker_only_context_short_circuits_with_actionable_error() {
        let ctx = ContextEntry {
            name: "lausanne-direct".into(),
            backend: Backend::Http {
                url: NO_CONTROLLER_SENTINEL.into(),
            },
            docker: Some("ssh://user@host".into()),
        };
        let err = match Session::from_context(ctx).await {
            Ok(_) => panic!("docker-only context should not yield a controller session"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("docker-only"), "msg: {msg}");
        assert!(msg.contains("lausanne-direct"), "msg: {msg}");
        assert!(
            !msg.contains("no-controller.invalid"),
            "sentinel leaked to operator: {msg}"
        );
    }
}
