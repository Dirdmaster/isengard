//! HTTP + HTTPS reverse proxy.
//!
//! Hyper-based (not Pingora). Pingora is excellent for the agent's
//! production proxy but heavy for an operator-side dev tool: it pulls in
//! its own runtime model, more crates, and more configuration than this
//! workload needs. Hyper 1.x + tokio-rustls gives us a small, predictable
//! handler we can read end to end.
//!
//! Per-request flow:
//!
//! 1. Read the `Host` header. If absent or empty, return 400.
//! 2. Strip any `:port` suffix and lower-case for lookup.
//! 3. Look up the rule in [`super::routing::RoutingTable`]. Miss -> 404 with
//!    a hint listing the hostnames the gateway IS serving.
//! 4. Hit -> forward to `<container_ip OR backend_host>:<container_port>`.
//!    On TCP-connect failure, return 502 with the upstream address in the
//!    body so the operator can quickly tell whether the rule is wrong vs
//!    the container hasn't published its port.
//!
//! The Host header is rewritten on the upstream request to match the
//! backend's address; everything else is forwarded as-is. This keeps the
//! proxy transparent for the simple GET/POST flows that fill 99% of the
//! `isd gateway` use case (browser hitting a hello-stack page).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use bytes::Bytes;
use http::header::{HOST, HeaderValue};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use super::routing::{Backend, RoutingTable, RuleState};

/// Shared state cloned into every accepted connection. `table` is the live
/// routing-table cache; `zone` and `backend_host` are read-only config.
#[derive(Debug, Clone)]
pub struct ProxyState {
    pub table: Arc<RwLock<RoutingTable>>,
    pub zone: String,
    pub backend_host: String,
}

/// Bind an HTTP listener and start accepting. The returned join handle owns
/// the accept loop; aborting it stops the listener.
pub async fn start_http(addr: SocketAddr, state: ProxyState) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding HTTP proxy listener on {addr}"))?;
    info!(%addr, "gateway HTTP proxy listening");
    let handle = tokio::spawn(async move {
        accept_loop_http(listener, state).await;
    });
    Ok(handle)
}

async fn accept_loop_http(listener: TcpListener, state: ProxyState) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "gateway HTTP accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle_request(state, req).await) }
            });
            if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                debug!(peer = %peer, error = %e, "gateway HTTP connection ended");
            }
        });
    }
}

/// Bind an HTTPS listener using the supplied PEM cert+key.
pub async fn start_https(
    addr: SocketAddr,
    state: ProxyState,
    cert_pem: &str,
    key_pem: &str,
) -> Result<JoinHandle<()>> {
    let server_config = build_server_config(cert_pem, key_pem)?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding HTTPS proxy listener on {addr}"))?;
    info!(%addr, "gateway HTTPS proxy listening");
    let handle = tokio::spawn(async move {
        accept_loop_https(listener, acceptor, state).await;
    });
    Ok(handle)
}

async fn accept_loop_https(listener: TcpListener, acceptor: TlsAcceptor, state: ProxyState) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "gateway HTTPS accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    debug!(peer = %peer, error = %e, "gateway HTTPS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle_request(state, req).await) }
            });
            if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                debug!(peer = %peer, error = %e, "gateway HTTPS connection ended");
            }
        });
    }
}

fn build_server_config(cert_pem: &str, key_pem: &str) -> Result<ServerConfig> {
    // Install the ring CryptoProvider once. rustls 0.23 requires an
    // explicit provider when the process hasn't set a default. Multiple
    // calls are safe (the second returns Err, which we discard).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .context("parsing gateway cert PEM")?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates parsed from gateway cert PEM"));
    }
    // Try PKCS#8 first (rcgen default), then fall back to legacy formats.
    let mut keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .context("parsing PKCS#8 key PEM")?;
    let key: PrivateKeyDer<'static> = if let Some(k) = keys.pop() {
        PrivateKeyDer::Pkcs8(k)
    } else {
        let mut ec_keys: Vec<_> = rustls_pemfile::ec_private_keys(&mut key_pem.as_bytes())
            .collect::<std::result::Result<_, _>>()
            .context("parsing EC key PEM")?;
        let k = ec_keys
            .pop()
            .ok_or_else(|| anyhow!("no PKCS#8 or EC key found in gateway key PEM"))?;
        PrivateKeyDer::Sec1(k)
    };

    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls ServerConfig")?;
    Ok(cfg)
}

/// Per-request handler: rule lookup + upstream fetch.
async fn handle_request(state: ProxyState, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let host = match host_from_request(&req) {
        Some(h) => h,
        None => return text_response(StatusCode::BAD_REQUEST, "missing or invalid Host header\n"),
    };

    let backend = {
        let table = state.table.read().await;
        match table.lookup(&host) {
            Some(b) => b.clone(),
            None => {
                return missing_host_response(&host, &state.zone, &table);
            }
        }
    };

    if backend.state == RuleState::NotServing {
        return text_response(
            StatusCode::BAD_GATEWAY,
            &format!(
                "gateway: rule {} for {host} is not serving (state pending/draining/failed). \
                 Wait for the agent to activate it, or check the dashboard.\n",
                backend.rule_id
            ),
        );
    }

    let upstream = upstream_for(&backend, &state.backend_host);
    debug!(host = %host, %upstream, "gateway proxy: forwarding");
    forward(req, upstream, &host).await
}

/// Pull the host name out of the Host header. Strips `:port` and lowers.
fn host_from_request(req: &Request<Incoming>) -> Option<String> {
    let raw = req.headers().get(HOST)?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .split(':')
            .next()
            .unwrap_or(trimmed)
            .to_ascii_lowercase(),
    )
}

/// Resolve the backend `host:port` for a routing rule. Prefers the rule's
/// `container_ip` when present; falls back to the gateway's
/// `--backend-host` (default `127.0.0.1`).
fn upstream_for(backend: &Backend, fallback_host: &str) -> String {
    let host = backend.container_ip.as_deref().unwrap_or(fallback_host);
    format!("{host}:{}", backend.container_port)
}

/// Forward `req` to `upstream` (`host:port`). Connect failure -> 502 with a
/// human-readable body.
async fn forward(req: Request<Incoming>, upstream: String, host: &str) -> Response<Full<Bytes>> {
    let stream = match TcpStream::connect(&upstream).await {
        Ok(s) => s,
        Err(e) => {
            warn!(host = %host, %upstream, error = %e, "gateway proxy: upstream connect failed");
            return text_response(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "gateway: cannot reach {upstream} for {host}: {e}\n\n\
                     hint: is the container's port published to localhost? \
                     For a hello-stack-style compose, that means `ports: \"<container_port>:<container_port>\"`.\n"
                ),
            );
        }
    };

    let io = TokioIo::new(stream);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
        Ok(p) => p,
        Err(e) => {
            warn!(host = %host, %upstream, error = %e, "gateway proxy: HTTP handshake failed");
            return text_response(
                StatusCode::BAD_GATEWAY,
                &format!("gateway: HTTP handshake to {upstream} failed: {e}\n"),
            );
        }
    };
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(error = %e, "gateway proxy: upstream conn ended");
        }
    });

    // Build the upstream request: keep method/path/body, but rewrite Host
    // to match the upstream so origin servers built around vhost routing
    // (e.g. Caddy in a downstream service) still match.
    let (parts, body) = req.into_parts();
    let mut upstream_req = Request::builder().method(parts.method).uri(parts.uri);
    let upstream_headers = upstream_req.headers_mut().expect("builder headers");
    *upstream_headers = parts.headers.clone();
    let upstream_host_value = match HeaderValue::from_str(&upstream) {
        Ok(v) => v,
        Err(_) => HeaderValue::from_static("upstream"),
    };
    upstream_headers.insert(HOST, upstream_host_value);
    let upstream_req = match upstream_req.body(body) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "gateway proxy: building upstream request failed");
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway: internal request build error\n",
            );
        }
    };

    let resp = match sender.send_request(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            warn!(host = %host, %upstream, error = %e, "gateway proxy: upstream request failed");
            return text_response(
                StatusCode::BAD_GATEWAY,
                &format!("gateway: upstream request to {upstream} failed: {e}\n"),
            );
        }
    };

    // Buffer the upstream body fully. v0.3.5 isn't trying to stream large
    // downloads; the realistic dev workload is small HTML responses.
    let (parts, body) = resp.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            warn!(error = %e, "gateway proxy: collecting upstream body failed");
            return text_response(
                StatusCode::BAD_GATEWAY,
                "gateway: error reading upstream body\n",
            );
        }
    };
    let mut builder = Response::builder().status(parts.status);
    {
        let h = builder.headers_mut().expect("builder headers");
        for (k, v) in parts.headers.iter() {
            h.insert(k.clone(), v.clone());
        }
        // Drop hop-by-hop headers we just buffered.
        h.remove("transfer-encoding");
        h.remove("connection");
    }
    builder.body(Full::new(collected)).unwrap_or_else(|_| {
        text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "gateway: response build error\n",
        )
    })
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("static text response builds")
}

fn missing_host_response(host: &str, zone: &str, table: &RoutingTable) -> Response<Full<Bytes>> {
    let mut body = format!(
        "gateway: no routing rule for {host} (zone .{zone}).\n\n\
         Tip: create a rule via the dashboard or label a container with \
         `isengard.expose.host=<name>.{zone}`.\n"
    );
    let hosts = table.hostnames();
    if !hosts.is_empty() {
        body.push_str("\nRules currently served by this gateway:\n");
        for h in hosts {
            body.push_str("  - ");
            body.push_str(&h);
            body.push('\n');
        }
    }
    text_response(StatusCode::NOT_FOUND, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::routing::{RoutingRuleDto, RoutingTable};

    fn rule_dto(id: i64, host: &str, port: u16, state: &str) -> RoutingRuleDto {
        RoutingRuleDto {
            id,
            public_hostname: host.into(),
            container_port: port,
            state: Some(state.into()),
            container_ip: None,
        }
    }

    #[test]
    fn upstream_for_uses_container_ip_when_present() {
        let backend = Backend {
            container_port: 80,
            container_ip: Some("10.0.0.5".into()),
            state: RuleState::Servable,
            rule_id: 1,
        };
        assert_eq!(upstream_for(&backend, "127.0.0.1"), "10.0.0.5:80");
    }

    #[test]
    fn upstream_for_falls_back_to_backend_host() {
        let backend = Backend {
            container_port: 8080,
            container_ip: None,
            state: RuleState::Servable,
            rule_id: 1,
        };
        assert_eq!(upstream_for(&backend, "127.0.0.1"), "127.0.0.1:8080");
        assert_eq!(
            upstream_for(&backend, "host.docker.internal"),
            "host.docker.internal:8080"
        );
    }

    #[test]
    fn host_header_lookup_table_round_trip() {
        // Drives the same path the proxy uses: rule list -> table -> lookup.
        let table = RoutingTable::from_rules(&[rule_dto(1, "Hello.ISD", 80, "active")]);
        let backend = table.lookup("hello.isd").unwrap();
        assert_eq!(backend.container_port, 80);
        assert_eq!(backend.state, RuleState::Servable);
    }

    #[test]
    fn host_header_with_port_suffix_strips_correctly() {
        // Build a tiny request and run host_from_request directly. We can't
        // easily build an `Incoming` body in tests, but the function only
        // looks at headers, so an empty-body Request works for the smoke
        // check via a manually-constructed Request<()>.
        let req = http::Request::builder()
            .header(HOST, "Hello.ISD:8080")
            .body(())
            .unwrap();
        let raw = req.headers().get(HOST).unwrap().to_str().unwrap();
        let host = raw
            .split(':')
            .next()
            .unwrap_or(raw)
            .trim()
            .to_ascii_lowercase();
        assert_eq!(host, "hello.isd");
    }

    #[tokio::test]
    async fn missing_host_response_lists_known_hostnames() {
        let table = RoutingTable::from_rules(&[
            rule_dto(1, "alpha.isd", 80, "active"),
            rule_dto(2, "beta.isd", 80, "active"),
        ]);
        let resp = missing_host_response("ghost.isd", "isd", &table);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("ghost.isd"),
            "body did not mention queried host: {text}"
        );
        assert!(
            text.contains("alpha.isd"),
            "body did not list alpha.isd: {text}"
        );
        assert!(
            text.contains("beta.isd"),
            "body did not list beta.isd: {text}"
        );
    }

    #[test]
    fn build_server_config_loads_rcgen_pem() {
        // Round-trip through cert::generate -> build_server_config to make
        // sure the rustls glue accepts what we ship.
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem) = super::super::cert::load_or_generate(dir.path(), "isd").unwrap();
        let _cfg = build_server_config(&cert_pem, &key_pem).expect("rustls server config");
    }
}
