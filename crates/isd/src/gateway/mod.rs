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
//! DNS query to 127.0.0.1:5300 (default)
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
//!  - DNS port: 5300 (unprivileged)
//!  - HTTP/HTTPS proxy ports: 8080 / 8443 (unprivileged); --port 80 / --tls-port 443 with sudo
//!  - TLS: self-signed via [`cert`], cached in app-config dir
//!  - Routing source: `GET /api/v1/routing/rules` initial pull plus a 30s poll. The
//!    spec calls for `/ws/events`-driven refresh; v0.3.5 implements polling first
//!    (simpler, deterministic) and leaves the WS path as a follow-up.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use tokio::sync::RwLock;

use crate::login::pinned_session;

pub mod cert;
pub mod dns;
pub mod proxy;
pub mod resolver_helper;
pub mod routing;

/// CLI arguments for `isd gateway`. See module-level docs for the locked
/// decisions behind every default.
#[derive(Debug, Args)]
pub struct GatewayArgs {
    /// Zone to resolve. Default `isd`. The DNS server answers `*.<zone>` ->
    /// 127.0.0.1; queries outside the zone are REFUSED so the OS resolver
    /// falls through to its upstream.
    #[arg(long, default_value = "isd")]
    pub zone: String,

    /// DNS listener port. Default 5300 (unprivileged). macOS routes here via
    /// `/etc/resolver/<zone>` containing `nameserver 127.0.0.1` + `port 5300`.
    #[arg(long, default_value_t = 5300)]
    pub dns_port: u16,

    /// HTTP proxy port. Default 8080. Use `--port 80` (with sudo) for the
    /// no-port-suffix browser experience.
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// HTTPS proxy port. Default 8443. Use `--tls-port 443` (with sudo) for
    /// the no-port-suffix HTTPS browser experience.
    #[arg(long, default_value_t = 8443)]
    pub tls_port: u16,

    /// Skip the HTTPS listener entirely (HTTP only). Useful in CI / sandbox
    /// where we don't want the rcgen-on-first-run prompt.
    #[arg(long)]
    pub no_tls: bool,

    /// Write `/etc/resolver/<zone>` and exit. Idempotent; requires sudo.
    #[arg(long)]
    pub install_resolver: bool,

    /// Remove `/etc/resolver/<zone>` and exit. Idempotent; requires sudo.
    #[arg(long)]
    pub uninstall_resolver: bool,

    /// Override the backend host the proxy forwards to. Default
    /// `127.0.0.1`, which assumes the operator publishes `<container_port>`
    /// 1:1 to localhost in their compose file. Set this to e.g.
    /// `host.docker.internal` if your runtime exposes containers on a
    /// different host alias.
    #[arg(long, default_value = "127.0.0.1")]
    pub backend_host: String,
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

/// Long-running gateway: DNS server + reverse proxy. Blocks on Ctrl+C.
pub async fn run_gateway(args: GatewayArgs, context: Option<&str>) -> Result<()> {
    let (ctx, client) = pinned_session(context).await?;

    // Initial routing-table pull. We don't fail hard if the controller is
    // unreachable: the operator may want to start the gateway first and
    // bring the controller up after. But we WARN so they know nothing will
    // resolve until refresh succeeds.
    let table = Arc::new(RwLock::new(routing::RoutingTable::default()));
    match routing::fetch_rules(&client, &ctx).await {
        Ok(rules) => {
            let built = routing::RoutingTable::from_rules(&rules);
            let n = built.len();
            *table.write().await = built;
            tracing::info!(rule_count = n, "gateway: initial routing pull");
            if n == 0 {
                println!(
                    "Loaded 0 routing rules from {} (table is empty; nothing will resolve until rules exist).",
                    ctx.controller_url
                );
            } else {
                println!("Loaded {n} routing rule(s) from {}.", ctx.controller_url);
            }
            // Use is_empty defensively so a zero-rule controller still prints
            // a useful banner instead of a misleading "Loaded 0 ...".
            if table.read().await.is_empty() {
                tracing::debug!("gateway: routing table is empty after initial pull");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "gateway: initial routing pull failed");
            eprintln!(
                "warning: could not reach controller at {} ({e:#}); the gateway \
                 will keep retrying every {}s.",
                ctx.controller_url,
                routing::REFRESH_INTERVAL.as_secs()
            );
        }
    }

    // Spawn the routing refresher. It owns the credentials clone + client so
    // the main task can move on to listener bring-up.
    let refresher_table = Arc::clone(&table);
    let refresher_ctx = ctx.clone();
    let refresher_client = client.clone();
    let refresher = tokio::spawn(async move {
        routing::refresh_loop(refresher_table, refresher_client, refresher_ctx).await;
    });

    // Bind the DNS server. 127.0.0.1 is the only sensible bind for a
    // dev-local resolver: macOS's /etc/resolver/<zone> contacts loopback,
    // and exposing the resolver on a LAN interface invites unintended
    // recursion / amplification.
    let dns_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.dns_port);
    let _dns_handle = dns::start(dns_addr, args.zone.clone())
        .await
        .with_context(|| format!("starting DNS server on {dns_addr}"))?;
    println!("DNS:   listening on {dns_addr} for *.{}", args.zone);

    // HTTP proxy listener.
    let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port);
    let proxy_state = proxy::ProxyState {
        table: Arc::clone(&table),
        zone: args.zone.clone(),
        backend_host: args.backend_host.clone(),
    };
    let _http_handle = proxy::start_http(http_addr, proxy_state.clone())
        .await
        .with_context(|| format!("binding HTTP proxy on {http_addr}"))?;
    println!("HTTP:  listening on {http_addr}");

    // HTTPS proxy listener (optional).
    let _https_handle: Option<tokio::task::JoinHandle<()>> = if args.no_tls {
        None
    } else {
        let cert_dir = cert::default_cert_dir()?;
        let (cert_pem, key_pem) = cert::load_or_generate(&cert_dir, &args.zone)?;
        let tls_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.tls_port);
        let handle = proxy::start_https(tls_addr, proxy_state.clone(), &cert_pem, &key_pem)
            .await
            .with_context(|| format!("binding HTTPS proxy on {tls_addr}"))?;
        println!("HTTPS: listening on {tls_addr} (self-signed; expect a browser warning)");
        Some(handle)
    };

    // Friendly nudge if the resolver isn't installed yet. We don't fail:
    // some operators want DNS only via `dig @127.0.0.1 -p 5300 ...` for
    // testing.
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
    println!("Gateway up. Press Ctrl+C to stop.");

    // Block until Ctrl+C. We let the OS reap our tasks on exit; the
    // listener handles drop themselves cleanly.
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down ...");
    refresher.abort();

    // Give a small drain window so the listeners can complete in-flight
    // requests. 250ms is more than enough for dev traffic; we don't block
    // on a more sophisticated graceful shutdown for v0.3.5.
    tokio::time::sleep(Duration::from_millis(250)).await;
    Ok(())
}
