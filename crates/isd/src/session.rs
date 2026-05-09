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

use anyhow::{Context as _, Result};

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
