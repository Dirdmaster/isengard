//! Pingora rustls cert resolver. Looks up SNI in the agent's `CertStore`.

use crate::tls::CertStore;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

pub struct IsengardCertResolver {
    store: Arc<CertStore>,
}

impl IsengardCertResolver {
    pub fn new(store: Arc<CertStore>) -> Self {
        Self { store }
    }
}

impl std::fmt::Debug for IsengardCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsengardCertResolver").finish()
    }
}

impl ResolvesServerCert for IsengardCertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?.to_string();
        let store = self.store.clone();
        // rustls calls resolve from the runtime that drives the TLS handshake
        // (Pingora's server runtime, also tokio). block_in_place is safe here
        // because we're inside a multi-thread runtime worker.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move { store.lookup(&sni).await })
        })
    }
}
