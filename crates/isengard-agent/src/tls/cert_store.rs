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
    /// Leaf certificate the SNI callback installs.
    pub leaf: X509,
    /// Intermediate chain certificates, root last.
    pub chain: Vec<X509>,
    /// Private key paired with `leaf`.
    pub key: PKey<Private>,
}

/// In-memory cert cache keyed by lowercased hostname. Cloning is cheap:
/// every clone shares the same underlying cache via `Arc`.
#[derive(Clone)]
pub struct CertStore {
    /// Disk-backed fallback used to seed and refresh the cache.
    storage: TlsStorage,
    /// `hostname -> entry` cache. Wrapped in `Arc<RwLock<_>>` so the SNI
    /// hot path takes a read lock while writes go through the controller
    /// push.
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
    /// Construct an empty store backed by `storage`. The cache is empty
    /// until [`Self::hydrate`] runs or a controller push installs an entry.
    pub fn new(storage: TlsStorage) -> Self {
        Self {
            storage,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Pre-populate the in-memory cache from disk. Called at agent boot so
    /// the very first SNI handshake after startup has the cert ready
    /// without waiting for the controller to push a `ProxyConfig`. Crucial
    /// for wildcard certs: the hostname-keyed cache miss + literal-only
    /// disk fallback in `lookup` would otherwise leave a freshly-booted
    /// agent unable to serve `*.foo.com` even though the file is on disk.
    ///
    /// Returns the number of certs hydrated. Never errors on missing
    /// storage dir; a malformed cert file logs a warn and is skipped so
    /// one bad file doesn't take the whole boot down.
    pub async fn hydrate(&self) -> Result<usize> {
        let hostnames = self
            .storage
            .list_hostnames()
            .await
            .context("listing tls storage hostnames")?;
        let mut loaded = 0;
        for hostname in hostnames {
            match self.load_from_disk(&hostname).await {
                Ok(entry) => {
                    self.cache.write().await.insert(hostname.clone(), entry);
                    loaded += 1;
                    tracing::info!(hostname = %hostname, "tls: cert hydrated from disk");
                }
                Err(e) => {
                    tracing::warn!(
                        hostname = %hostname,
                        error = %e,
                        "tls: cert hydrate failed; skipping (file may be corrupt or partially written)",
                    );
                }
            }
        }
        Ok(loaded)
    }

    /// Look up a `CertEntry` by SNI hostname. Cache-first, then disk; finally
    /// a wildcard match against any cached entry whose key starts with `*.`.
    ///
    /// Wildcard match obeys RFC 6125: `*.foo.com` covers `bar.foo.com` (one
    /// label deeper) but not `foo.com` (the apex itself; that needs a
    /// dedicated entry, which the controller always installs alongside any
    /// wildcard) and not `a.b.foo.com` (two labels deeper).
    pub async fn lookup(&self, hostname: &str) -> Option<Arc<CertEntry>> {
        if let Some(e) = self.cache.read().await.get(hostname).cloned() {
            return Some(e);
        }
        // Wildcard match before disk: certs received via `ProxyConfig` are
        // installed in the cache under each SAN. A SNI for `home.vallee.casa`
        // won't have a literal entry but `*.vallee.casa` will, and that's the
        // cert that should serve.
        if let Some(e) = self.lookup_wildcard(hostname).await {
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
            Err(e) => {
                // Log at warn so a corrupt cert is observable in production
                // logs (otherwise lookups silently miss and the cert callback
                // declines, with no signal pointing at the actual problem).
                tracing::warn!(
                    hostname = %hostname,
                    error = %e,
                    "tls: cert load from disk failed"
                );
                None
            }
        }
    }

    /// Walk the cache looking for any `*.<base>` key where `<base>` is the
    /// parent zone of `hostname` (one DNS label up). Returns on first hit.
    async fn lookup_wildcard(&self, hostname: &str) -> Option<Arc<CertEntry>> {
        let parent = match hostname.split_once('.') {
            Some((_, rest)) if rest.contains('.') => rest,
            // No parent zone or single-label hostname: nothing to match against.
            _ => return None,
        };
        let candidate = format!("*.{parent}");
        self.cache.read().await.get(&candidate).cloned()
    }

    /// Install a new cert for `hostname`. Parses the PEM material BEFORE
    /// writing to disk so a malformed input is rejected without leaving
    /// half-state on disk; updates the cache last so a parse failure can't
    /// leave cache and disk disagreeing.
    pub async fn install(&self, hostname: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
        let entry =
            parse_entry(cert_pem, key_pem).with_context(|| format!("parse cert for {hostname}"))?;
        self.storage.write(hostname, cert_pem, key_pem).await?;
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

    /// Internal helper: load from disk.
    async fn load_from_disk(&self, hostname: &str) -> Result<Arc<CertEntry>> {
        let files = self.storage.read(hostname).await?;
        let entry = parse_entry(&files.cert_pem, &files.key_pem)?;
        Ok(Arc::new(entry))
    }
}

/// Internal helper: parse entry.
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
