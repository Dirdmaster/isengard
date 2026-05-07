//! v0.3.5: `isd gateway`: DNS resolver + reverse proxy on the operator's
//! Mac that bridges browser traffic to containerized stacks.
//!
//! mDNS in Docker on macOS is broken by design (the agent's responder
//! broadcasts on the Docker bridge inside the OrbStack/Docker Desktop VM;
//! macOS Bonjour listens on real network interfaces; the packets never
//! cross). This subcommand runs a small loopback DNS server + reverse proxy
//! on the host so browsers reach `*.isd` (or any configured zone) without
//! /etc/hosts edits or mDNS plumbing.
//!
//! Architecture:
//!
//! ```text
//! browser  ->  http(s)://hello.isd
//!    |
//!    v
//! macOS resolver picks up `.isd` via /etc/resolver/isd
//!    |
//!    v
//! DNS query to 127.0.0.1:5350 (default)
//!    |
//!    v
//! [`dns`] returns 127.0.0.1
//!    |
//!    v
//! browser connects to 127.0.0.1:8080 (or :8443)
//!    |
//!    v
//! [`proxy`] reads Host header, forwards to backend per [`routing`]
//! ```
//!
//! Locked decisions (see `1 Projects/Isengard/v0.3.5 isd gateway.md`):
//!  - Zone default: `isd`
//!  - DNS port: 5350 (unprivileged)
//!  - HTTP/HTTPS proxy ports: 8080 / 8443 (unprivileged); --port 80 / --tls-port 443 with sudo
//!  - TLS: self-signed via [`cert`], cached in app-config dir
//!  - Routing source: `GET /api/v1/routing/rules` initial pull plus a 30s poll. The
//!    spec calls for `/ws/events`-driven refresh; v0.3.5 implements polling first
//!    (simpler, deterministic) and leaves the WS path as a follow-up.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context as _, Result, anyhow};
use clap::Args;

pub mod dns;
pub mod resolver_helper;

// Proxy + routing-table + cert helpers were stripped after #99 published the
// agent's Pingora to host:80/443 via the shared `isengard-proxy` network.
// The browser now hits Pingora directly, which already does Host-header
// routing + healthchecks. The gateway shrinks to a DNS responder + the
// /etc/resolver/<zone> install helper.

/// CLI arguments for `isd gateway`. DNS-only after #99 made Pingora
/// directly reachable on host :80 / :443 via the shared isengard-proxy
/// network.
#[derive(Debug, Args)]
pub struct GatewayArgs {
    /// Zone to resolve. Default `isd`. The DNS server answers `*.<zone>` ->
    /// 127.0.0.1; queries outside the zone are REFUSED so the OS resolver
    /// falls through to its upstream.
    #[arg(long, default_value = "isd")]
    pub zone: String,

    /// DNS listener port. Default 5350 (unprivileged). macOS routes here via
    /// `/etc/resolver/<zone>` containing `nameserver 127.0.0.1` + `port 5350`.
    #[arg(long, default_value_t = 5350)]
    pub dns_port: u16,

    /// Write `/etc/resolver/<zone>` and exit. Idempotent; requires sudo.
    #[arg(long)]
    pub install_resolver: bool,

    /// Remove `/etc/resolver/<zone>` and exit. Idempotent; requires sudo.
    #[arg(long)]
    pub uninstall_resolver: bool,
}

/// Top-level entry point for the `Gateway` subcommand. Branches into the
/// install / uninstall helpers (one-shot) or the long-running gateway.
pub async fn run(args: GatewayArgs, context: Option<&str>) -> Result<()> {
    if args.install_resolver && args.uninstall_resolver {
        return Err(anyhow!(
            "--install-resolver and --uninstall-resolver are mutually exclusive"
        ));
    }
    if args.install_resolver {
        return resolver_helper::install(&args.zone, args.dns_port);
    }
    if args.uninstall_resolver {
        return resolver_helper::uninstall(&args.zone);
    }

    run_gateway(args, context).await
}

/// Long-running gateway: DNS server only. Blocks on Ctrl+C. After #99 the
/// agent's Pingora is reachable on host :80 / :443 directly, so the proxy
/// half of the original v0.3.5 plan is gone.
pub async fn run_gateway(args: GatewayArgs, _context: Option<&str>) -> Result<()> {
    // Bind the DNS server. 127.0.0.1 is the only sensible bind for a
    // dev-local resolver: macOS's /etc/resolver/<zone> contacts loopback,
    // and exposing the resolver on a LAN interface invites unintended
    // recursion / amplification.
    let dns_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.dns_port);
    let _dns_handle = dns::start(dns_addr, args.zone.clone())
        .await
        .with_context(|| format!("starting DNS server on {dns_addr}"))?;
    println!("DNS: listening on {dns_addr} for *.{}", args.zone);

    if !resolver_helper::is_installed(&args.zone) {
        println!();
        println!("note: /etc/resolver/{} is not installed.", args.zone);
        println!(
            "      run `sudo isd gateway --install-resolver --zone {}` once",
            args.zone
        );
        println!("      to make macOS route `*.{}` queries here.", args.zone);
    }

    println!();
    println!(
        "Gateway up. Open `http://<name>.{}` (port 80) in your browser; \
         that lands at the agent's pingora via the published port.",
        args.zone
    );
    println!("Press Ctrl+C to stop.");

    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down ...");
    Ok(())
}
