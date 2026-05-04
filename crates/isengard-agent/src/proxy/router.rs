//! `ProxyHttp` implementation — the per-request routing logic.
//!
//! Phase 8b (Task 9): single-rule host-header routing. The router reads the
//! incoming `Host` header, looks it up in `ProxyState.upstreams`, and returns
//! an `HttpPeer` (or a `404`/`503` Pingora error). SNI-based lookup for the
//! HTTPS listener lands in a later task.

use async_trait::async_trait;
use pingora_core::Result;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};

use super::ProxyState;

/// The Isengard reverse-proxy implementation.
pub struct IsengardProxy {
    pub state: ProxyState,
}

impl IsengardProxy {
    pub fn new(state: ProxyState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ProxyHttp for IsengardProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    /// Short-circuit `/.well-known/acme-challenge/<token>` requests on the
    /// :8080 HTTP listener. Returns `Ok(true)` when we've fully written the
    /// response (so Pingora skips upstream lookup). Unknown tokens get a 404
    /// — the ACME server treats both as failures, but distinguishing in logs
    /// is useful when triaging an order.
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path();
        const PREFIX: &str = "/.well-known/acme-challenge/";
        if let Some(token) = path.strip_prefix(PREFIX) {
            let token = token.to_string();
            let key_auth = self.state.acme_challenges.lookup(&token).await;
            let body = key_auth.unwrap_or_default();
            let status = if body.is_empty() { 404 } else { 200 };

            let mut resp = pingora_http::ResponseHeader::build(status, None)?;
            resp.insert_header("Content-Type", "text/plain")?;
            resp.insert_header("Content-Length", body.len().to_string())?;
            session.write_response_header(Box::new(resp), false).await?;
            session
                .write_response_body(Some(bytes::Bytes::from(body.into_bytes())), true)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
            .unwrap_or_default();

        if host.is_empty() {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(400),
                "missing_host",
                "request has no Host header (or it failed UTF-8 decode)",
            ));
        }

        let upstreams = self.state.upstreams.read().await;
        let Some(up) = upstreams.get(&host) else {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(404),
                "no_route",
                format!("no routing rule for {host}"),
            ));
        };
        if !up.healthy {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(503),
                "no_healthy_upstream",
                format!("no healthy upstream for {host}"),
            ));
        }
        if up.state == crate::proxy::UpstreamState::Draining {
            return Err(pingora_core::Error::because(
                pingora_core::ErrorType::HTTPStatus(503),
                "upstream_draining",
                format!("upstream for {host} is draining"),
            ));
        }
        // Pingora 0.4 quirk: even with tls=false the SNI string doubles as the
        // upstream Host header. Empty SNI causes Pingora to 502 without
        // attempting the upstream connection. Pass the original public hostname.
        Ok(Box::new(HttpPeer::new(up.addr, false, host)))
    }
}
