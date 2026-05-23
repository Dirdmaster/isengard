//! [`IsengardResolver`]: glue between a [`ZoneSource`] and hickory.
//!
//! The resolver owns one zone source plus one upstream [`TokioResolver`]
//! and stands up `hickory_server::ServerFuture` on UDP and/or TCP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use hickory_server::ServerFuture;
use tokio::net::{TcpListener, UdpSocket};
use tracing::info;

use crate::handler::RequestHandlerImpl;
use crate::zone_source::ZoneSource;

/// Default TCP accept timeout for hickory's [`ServerFuture::register_listener`].
///
/// hickory drops idle TCP connections after this; clients reconnect when
/// they need to. 5 seconds matches the example in hickory's own docs.
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// A hickory-backed DNS resolver wired to one [`ZoneSource`] plus a list
/// of upstream forwarders.
///
/// Construct with [`new`](Self::new), then drive with [`serve_udp`](Self::serve_udp),
/// [`serve_tcp`](Self::serve_tcp), or [`serve`](Self::serve) for both at
/// once. Each `serve_*` call binds a fresh socket, owns a fresh
/// `ServerFuture`, and runs until cancelled by the caller's runtime.
pub struct IsengardResolver {
    /// Authoritative source. `Arc` so the per-connection handler clones
    /// share it.
    zones: Arc<dyn ZoneSource + Send + Sync>,
    /// Upstream forwarder (1.1.1.1, ISP DNS, etc.). `Arc<TokioResolver>`
    /// lets the request handler clone cheaply.
    upstream: Arc<TokioResolver>,
}

impl std::fmt::Debug for IsengardResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ZoneSource` is dyn (no Debug bound); reach for what we *can*
        // print so callers using `unwrap_err`-style assertions still get
        // a useful diagnostic.
        f.debug_struct("IsengardResolver")
            .field("zones", &"<dyn ZoneSource>")
            .field("upstream", &"<TokioResolver>")
            .finish()
    }
}

impl IsengardResolver {
    /// Build a resolver wired to `zones` and forwarding misses to
    /// `upstream_addrs`.
    ///
    /// Upstreams are tried in order. Both UDP and TCP entries are added
    /// for each address: hickory falls back to TCP automatically on
    /// truncated UDP responses.
    ///
    /// # Errors
    ///
    /// Fails if `upstream_addrs` is empty: a resolver that can't forward
    /// is silently broken for any name outside the zones.
    pub fn new(
        zones: Arc<dyn ZoneSource + Send + Sync>,
        upstream_addrs: Vec<SocketAddr>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !upstream_addrs.is_empty(),
            "at least one upstream address is required: \
             a resolver without forwarders cannot answer anything outside its zones",
        );

        let mut config = ResolverConfig::new();
        for addr in upstream_addrs {
            config.add_name_server(NameServerConfig {
                socket_addr: addr,
                protocol: Protocol::Udp,
                tls_dns_name: None,
                http_endpoint: None,
                trust_negative_responses: true,
                bind_addr: None,
            });
            config.add_name_server(NameServerConfig {
                socket_addr: addr,
                protocol: Protocol::Tcp,
                tls_dns_name: None,
                http_endpoint: None,
                trust_negative_responses: true,
                bind_addr: None,
            });
        }

        // Forwarder defaults tuned for a per-host LAN resolver, NOT for the
        // wider-internet resolver shape hickory ships out of the box:
        //
        // - 2-second per-attempt timeout: an in-LAN upstream that takes more
        //   than that is broken; better to fall through than hold the client.
        // - 2 attempts: original + one retry across the redundant upstreams
        //   we register (UDP + TCP per address). Beyond that we're just
        //   stacking latency on a sick upstream.
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(2);
        opts.attempts = 2;

        let upstream =
            TokioResolver::builder_with_config(config, TokioConnectionProvider::default())
                .with_options(opts)
                .build();

        Ok(Self {
            zones,
            upstream: Arc::new(upstream),
        })
    }

    /// Bind UDP on `addr` and serve until the future is cancelled.
    ///
    /// # Errors
    ///
    /// Fails when the UDP bind fails (port in use, permission denied,
    /// etc.) or when hickory's shutdown returns an error.
    pub async fn serve_udp(&self, addr: SocketAddr) -> Result<()> {
        let socket = UdpSocket::bind(addr)
            .await
            .with_context(|| format!("binding DNS UDP socket on {addr}"))?;
        info!(%addr, "isengard-dns: UDP listening");

        let mut server = ServerFuture::new(self.handler());
        server.register_socket(socket);
        server
            .block_until_done()
            .await
            .context("hickory ServerFuture exited with error")?;
        Ok(())
    }

    /// Bind TCP on `addr` and serve until cancelled.
    ///
    /// # Errors
    ///
    /// Fails when the TCP bind fails or hickory's shutdown returns an
    /// error.
    pub async fn serve_tcp(&self, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding DNS TCP listener on {addr}"))?;
        info!(%addr, "isengard-dns: TCP listening");

        let mut server = ServerFuture::new(self.handler());
        server.register_listener(listener, TCP_TIMEOUT);
        server
            .block_until_done()
            .await
            .context("hickory ServerFuture exited with error")?;
        Ok(())
    }

    /// Bind UDP on `udp_addr` and TCP on `tcp_addr` on one `ServerFuture`
    /// and serve until cancelled.
    ///
    /// Preferred over running [`serve_udp`](Self::serve_udp) and
    /// [`serve_tcp`](Self::serve_tcp) on separate tasks: one server
    /// instance keeps query telemetry coherent and exits cleanly.
    ///
    /// # Errors
    ///
    /// Fails when either bind fails or hickory exits with an error.
    pub async fn serve(&self, udp_addr: SocketAddr, tcp_addr: SocketAddr) -> Result<()> {
        let udp = UdpSocket::bind(udp_addr)
            .await
            .with_context(|| format!("binding DNS UDP socket on {udp_addr}"))?;
        let tcp = TcpListener::bind(tcp_addr)
            .await
            .with_context(|| format!("binding DNS TCP listener on {tcp_addr}"))?;
        info!(%udp_addr, %tcp_addr, "isengard-dns: UDP + TCP listening");

        let mut server = ServerFuture::new(self.handler());
        server.register_socket(udp);
        server.register_listener(tcp, TCP_TIMEOUT);
        server
            .block_until_done()
            .await
            .context("hickory ServerFuture exited with error")?;
        Ok(())
    }

    /// Build a fresh [`RequestHandlerImpl`] for one `ServerFuture`.
    fn handler(&self) -> RequestHandlerImpl {
        RequestHandlerImpl::new(self.zones.clone(), self.upstream.clone())
    }

    /// Direct hook into the handler. Tests use this to exercise the
    /// authoritative / forwarding path without binding a socket.
    #[must_use]
    pub fn handler_for_test(&self) -> RequestHandlerImpl {
        self.handler()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StaticZoneSource;

    #[test]
    fn empty_upstream_addrs_rejected() {
        let src = Arc::new(StaticZoneSource::builder().build());
        let err = IsengardResolver::new(src, Vec::new()).unwrap_err();
        assert!(
            err.to_string().contains("at least one upstream"),
            "error should explain why empty upstreams are rejected: {err}",
        );
    }

    #[test]
    fn new_with_upstream_addrs_succeeds() {
        let src = Arc::new(StaticZoneSource::builder().build());
        let r = IsengardResolver::new(
            src,
            vec!["1.1.1.1:53".parse().unwrap(), "1.0.0.1:53".parse().unwrap()],
        );
        assert!(r.is_ok());
    }
}
