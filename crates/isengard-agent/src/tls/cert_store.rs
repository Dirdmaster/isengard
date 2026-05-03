//! Process-wide cache of (leaf X509, chain Vec<X509>, PKey) entries indexed
//! by hostname (SNI). Backed by `TlsStorage` (filesystem). Cache is
//! read-mostly; mutations happen on cert install / renewal.

use crate::tls::storage::TlsStorage;
use anyhow::{Context, Result};
use pingora_boringssl::pkey::{PKey, Private};
use pingora_boringssl::x509::X509;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Parsed cert material ready to install on a boringssl `SslRef` during the
/// TLS handshake. `leaf` goes via `set_certificate`, `chain` via
/// `add_chain_cert`, `key` via `set_private_key`.
#[derive(Clone)]
pub struct CertEntry {
    pub leaf: X509,
    pub chain: Vec<X509>,
    pub key: PKey<Private>,
}

#[derive(Clone)]
pub struct CertStore {
    storage: TlsStorage,
    cache: Arc<RwLock<HashMap<String, Arc<CertEntry>>>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // CertEntry contains private key material; do not format the cache.
        f.debug_struct("CertStore")
            .field("storage", &self.storage)
            .finish_non_exhaustive()
    }
}

impl CertStore {
    pub fn new(storage: TlsStorage) -> Self {
        Self {
            storage,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Look up a `CertEntry` by SNI hostname. Cache-first, then disk.
    pub async fn lookup(&self, hostname: &str) -> Option<Arc<CertEntry>> {
        if let Some(e) = self.cache.read().await.get(hostname).cloned() {
            return Some(e);
        }
        match self.load_from_disk(hostname).await {
            Ok(e) => {
                self.cache
                    .write()
                    .await
                    .insert(hostname.to_string(), e.clone());
                Some(e)
            }
            Err(_) => None,
        }
    }

    /// Install a new cert for `hostname`: writes to disk + updates cache.
    pub async fn install(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        self.storage.write(hostname, cert_pem, key_pem).await?;
        let entry = parse_entry(cert_pem, key_pem)?;
        self.cache
            .write()
            .await
            .insert(hostname.to_string(), Arc::new(entry));
        Ok(())
    }

    /// Drop a hostname's cert from cache + disk.
    pub async fn remove(&self, hostname: &str) -> Result<()> {
        self.cache.write().await.remove(hostname);
        self.storage.delete(hostname).await
    }

    async fn load_from_disk(&self, hostname: &str) -> Result<Arc<CertEntry>> {
        let files = self.storage.read(hostname).await?;
        let entry = parse_entry(&files.cert_pem, &files.key_pem)?;
        Ok(Arc::new(entry))
    }
}

fn parse_entry(cert_pem: &str, key_pem: &str) -> Result<CertEntry> {
    let mut chain = X509::stack_from_pem(cert_pem.as_bytes())
        .context("parsing cert PEM (X509::stack_from_pem)")?;
    if chain.is_empty() {
        return Err(anyhow::anyhow!("cert PEM contains no certificates"));
    }
    let leaf = chain.remove(0);
    let key = PKey::private_key_from_pem(key_pem.as_bytes()).context("parsing key PEM")?;
    Ok(CertEntry { leaf, chain, key })
}
