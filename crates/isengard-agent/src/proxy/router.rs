//! `ProxyHttp` implementation — the per-request routing logic.
//!
//! Phase 8b stub: every request returns an error. Task 9 fills in
//! `upstream_peer` to read host/SNI, look up in `ProxyState.upstreams`, and
//! return an `HttpPeer`.

use async_trait::async_trait;
use pingora_core::Result;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};

use super::ProxyState;

/// The Isengard reverse-proxy implementation.
pub struct IsengardProxy {
    #[allow(dead_code)] // Task 9 wires this up
    pub(crate) state: ProxyState,
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
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Err(pingora_core::Error::new_str("not implemented"))
    }
}
