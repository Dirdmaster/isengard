//! Pingora boringssl cert callback. Looks up SNI in the agent's `CertStore`
//! and installs the matching cert/key on the SslRef during the TLS handshake.

use crate::tls::CertStore;
use async_trait::async_trait;
use pingora_boringssl::ssl::{NameType, SslRef};
use pingora_core::listeners::TlsAccept;
use std::sync::Arc;
use tracing::{debug, warn};

pub struct IsengardCertCallback {
    store: Arc<CertStore>,
}

impl IsengardCertCallback {
    pub fn new(store: Arc<CertStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TlsAccept for IsengardCertCallback {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let sni = match ssl.servername(NameType::HOST_NAME) {
            Some(s) => s.to_string(),
            None => {
                debug!("tls: handshake without SNI; declining cert");
                return;
            }
        };

        let Some(entry) = self.store.lookup(&sni).await else {
            warn!(sni = %sni, "tls: no cert for SNI; handshake will fail");
            return;
        };

        if let Err(e) = ssl.set_certificate(&entry.leaf) {
            warn!(sni = %sni, error = %e, "tls: set_certificate failed");
            return;
        }
        for c in &entry.chain {
            if let Err(e) = ssl.add_chain_cert(c) {
                warn!(sni = %sni, error = %e, "tls: add_chain_cert failed");
            }
        }
        if let Err(e) = ssl.set_private_key(&entry.key) {
            warn!(sni = %sni, error = %e, "tls: set_private_key failed");
        }
    }
}
