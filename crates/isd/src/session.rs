//! Per-command session: resolves a [`ContextEntry`] into a usable
//! `reqwest::Client` + base URL + (when SSH-backed) live tunnel.
//!
//! Replaces the old `login::pinned_session` which assumed every context
//! was a directly-reachable URL. The session owns the tunnel for its
//! lifetime; dropping the session closes the tunnel.
//!
//! Subcommands now take `&Session` instead of `(&ContextEntry,
//! &reqwest::Client)`. The base URL is `session.controller_url()`; for
//! HTTP backends that's the saved URL, for SSH backends it's the local
//! loopback port the tunnel forwards to.

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
    controller_url: String,
    /// SSH tunnel held alive for the session. `None` for HTTP backends.
    /// Field is read via `Drop` only; allow_dead_code keeps clippy quiet.
    #[allow(dead_code)]
    tunnel: Option<Tunnel>,
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
        let (controller_url, tunnel) = match &ctx.backend {
            Backend::Http { url } => (url.trim_end_matches('/').to_string(), None),
            Backend::Ssh {
                target,
                dashboard_port,
            } => {
                let tunnel = Tunnel::open(target, *dashboard_port)
                    .await
                    .with_context(|| {
                        format!("opening SSH tunnel to {target} (context {:?})", ctx.name)
                    })?;
                (tunnel.local_url(), Some(tunnel))
            }
            // Track D Phase 4: connect to docker via the configured URL,
            // discover the controller container by label, and (for SSH-
            // backed docker URLs) open a second LocalForward via the
            // existing ControlMaster so reqwest can hit the controller's
            // published REST port over loopback.
            Backend::Docker { url } => {
                let backend = isd_runtime::DockerBackend::from_uri(url)
                    .await
                    .with_context(|| {
                        format!("connecting to docker at {url} for context {:?}", ctx.name)
                    })?;
                let endpoint =
                    isd_runtime::discover(backend.client())
                        .await
                        .with_context(|| {
                            format!("discovering isengard controller via docker at {url}",)
                        })?;
                if let Some(target) = url.strip_prefix("ssh://") {
                    // ssh://[user@]host[:port][/path] -> [user@]host[:port].
                    // docker's URL form does not carry a path, but strip
                    // defensively in case an operator pasted one.
                    let target = target.split('/').next().unwrap_or(target);
                    let tunnel =
                        Tunnel::open_local_forward(target, &endpoint.host_ip, endpoint.host_port)
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
                    let url = format!("http://{}:{}", endpoint.host_ip, endpoint.host_port);
                    (url, None)
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
        })
    }

    /// Base URL for REST calls. Subcommands append `/api/v1/...`.
    pub fn controller_url(&self) -> &str {
        &self.controller_url
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
    async fn docker_backend_session_open_fails_when_no_docker_available() {
        // Without a real docker socket the test asserts the error path
        // (we don't have docker in CI). The error must reference docker
        // so the operator sees a useful breadcrumb (vs. a generic "could
        // not connect" lower in the stack).
        let ctx = ContextEntry {
            name: "lausanne".into(),
            backend: Backend::Docker {
                url: "unix:///nonexistent/docker.sock".into(),
            },
            docker: None,
        };
        let err = match Session::from_context(ctx).await {
            Ok(_) => panic!("expected error when docker socket is unreachable"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("docker"), "msg: {msg}");
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
