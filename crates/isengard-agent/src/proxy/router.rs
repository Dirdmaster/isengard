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
        Ok(Box::new(HttpPeer::new(up.addr, false, String::new())))
    }
}
