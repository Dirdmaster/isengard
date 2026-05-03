//! Process-wide cache of `CertifiedKey` indexed by hostname (SNI).
//! Backed by `TlsStorage` (filesystem). Cache is read-mostly; mutations
//! happen on cert install / renewal and are explicit.

use crate::tls::storage::TlsStorage;
use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::{CertifiedKey, SigningKey};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct CertStore {
    storage: TlsStorage,
    cache: Arc<RwLock<HashMap<String, Arc<CertifiedKey>>>>,
}

impl CertStore {
    pub fn new(storage: TlsStorage) -> Self {
        Self {
            storage,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Look up a `CertifiedKey` by SNI hostname. Cache-first, then disk.
    pub async fn lookup(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
        if let Some(k) = self.cache.read().await.get(hostname).cloned() {
            return Some(k);
        }
        match self.load_from_disk(hostname).await {
            Ok(k) => {
                self.cache
                    .write()
                    .await
                    .insert(hostname.to_string(), k.clone());
                Some(k)
            }
            Err(_) => None,
        }
    }

    /// Install a new cert for `hostname`: writes to disk + updates cache.
    pub async fn install(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        self.storage.write(hostname, cert_pem, key_pem).await?;
        let key = parse_certified_key(cert_pem, key_pem)?;
        self.cache
            .write()
            .await
            .insert(hostname.to_string(), Arc::new(key));
        Ok(())
    }

    /// Drop a hostname's cert from cache + disk.
    pub async fn remove(&self, hostname: &str) -> Result<()> {
        self.cache.write().await.remove(hostname);
        self.storage.delete(hostname).await
    }

    async fn load_from_disk(&self, hostname: &str) -> Result<Arc<CertifiedKey>> {
        let files = self.storage.read(hostname).await?;
        let key = parse_certified_key(&files.cert_pem, &files.key_pem)?;
        Ok(Arc::new(key))
    }
}

fn parse_certified_key(cert_pem: &str, key_pem: &str) -> Result<CertifiedKey> {
    let mut cert_reader = std::io::BufReader::new(cert_pem.as_bytes());
    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing cert PEM")?;
    if cert_chain.is_empty() {
        return Err(anyhow!("cert PEM contains no certificates"));
    }

    let mut key_reader = std::io::BufReader::new(key_pem.as_bytes());
    let key_der: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .context("parsing key PEM")?
        .ok_or_else(|| anyhow!("no private key found in PEM"))?;

    let signing_key: Arc<dyn SigningKey> = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .map_err(|e| anyhow!("unsupported key type: {e}"))?;
    Ok(CertifiedKey::new(cert_chain, signing_key))
}
