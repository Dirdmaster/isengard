//! In-memory store for issued wildcard certs. The controller hands out
//! current cert material to agents via `RoutingPusher::push_to_host` (see
//! `routing.rs`); this struct is the source of truth.
//!
//! Keyed by primary identifier (e.g. `*.vallee.casa`). All other identifiers
//! covered by the same cert (the apex `vallee.casa`) are tracked alongside so
//! the lookup-by-hostname path can match either.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct WildcardCert {
    /// All SANs covered by this cert. The first is the primary key.
    pub identifiers: Vec<String>,
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Debug, Default)]
pub struct WildcardCertStore {
    inner: RwLock<HashMap<String, Arc<WildcardCert>>>,
}

impl WildcardCertStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a freshly-issued cert. Replaces any existing cert under the
    /// same primary identifier.
    pub async fn install(
        &self,
        identifiers: &[String],
        cert_pem: &str,
        key_pem: &str,
    ) -> Result<()> {
        if identifiers.is_empty() {
            return Err(anyhow::anyhow!("install: identifiers must be non-empty"));
        }
        let entry = Arc::new(WildcardCert {
            identifiers: identifiers.to_vec(),
            cert_pem: cert_pem.to_string(),
            key_pem: key_pem.to_string(),
        });
        let mut guard = self.inner.write().await;
        guard.insert(identifiers[0].clone(), entry);
        Ok(())
    }

    /// Snapshot every cert in the store. Used by the routing pusher to
    /// include current cert material in `ProxyConfig` pushes to agents.
    pub async fn snapshot(&self) -> Vec<Arc<WildcardCert>> {
        let guard = self.inner.read().await;
        guard.values().cloned().collect()
    }

    /// Look up the cert that would be served for `hostname` (matching either
    /// the apex `foo.com` or any subdomain of `*.foo.com`).
    ///
    /// Wildcard rule per RFC 6125 §6.4.3: `*.foo` matches any single-label
    /// subdomain (`bar.foo` yes, `bar.baz.foo` no).
    pub async fn lookup_for_hostname(&self, hostname: &str) -> Option<Arc<WildcardCert>> {
        let lc = hostname.to_lowercase();
        let guard = self.inner.read().await;
        for cert in guard.values() {
            for id in &cert.identifiers {
                let id_lc = id.to_lowercase();
                if id_lc == lc {
                    return Some(cert.clone());
                }
                if let Some(suffix) = id_lc.strip_prefix("*.") {
                    // `*.foo.com` matches `bar.foo.com` (one label deeper),
                    // not `foo.com` (the apex itself) and not
                    // `bar.baz.foo.com` (two labels deeper).
                    if let Some(prefix) = lc.strip_suffix(&format!(".{suffix}")) {
                        if !prefix.is_empty() && !prefix.contains('.') {
                            return Some(cert.clone());
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pem(tag: &str) -> String {
        format!("-----BEGIN CERTIFICATE-----\n{tag}\n-----END CERTIFICATE-----\n")
    }

    #[tokio::test]
    async fn install_then_snapshot_round_trips() {
        let store = WildcardCertStore::new();
        store
            .install(
                &["*.vallee.casa".into(), "vallee.casa".into()],
                &fake_pem("cert"),
                &fake_pem("key"),
            )
            .await
            .unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].identifiers[0], "*.vallee.casa");
    }

    #[tokio::test]
    async fn install_replaces_existing_key() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.x.com".into()], &fake_pem("v1"), &fake_pem("k1"))
            .await
            .unwrap();
        store
            .install(&["*.x.com".into()], &fake_pem("v2"), &fake_pem("k2"))
            .await
            .unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert!(snap[0].cert_pem.contains("v2"));
    }

    #[tokio::test]
    async fn install_empty_identifiers_errors() {
        let store = WildcardCertStore::new();
        let err = store.install(&[], "c", "k").await.unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn lookup_matches_apex() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.vallee.casa".into(), "vallee.casa".into()], "c", "k")
            .await
            .unwrap();
        let hit = store.lookup_for_hostname("vallee.casa").await;
        assert!(hit.is_some());
    }

    #[tokio::test]
    async fn lookup_matches_subdomain_of_wildcard() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.vallee.casa".into()], "c", "k")
            .await
            .unwrap();
        let hit = store.lookup_for_hostname("home.vallee.casa").await;
        assert!(hit.is_some());
        let hit = store.lookup_for_hostname("api.vallee.casa").await;
        assert!(hit.is_some());
    }

    #[tokio::test]
    async fn lookup_rejects_nested_subdomain() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.vallee.casa".into()], "c", "k")
            .await
            .unwrap();
        // RFC 6125: wildcard covers exactly one label.
        let hit = store.lookup_for_hostname("a.b.vallee.casa").await;
        assert!(hit.is_none());
    }

    #[tokio::test]
    async fn lookup_rejects_unrelated_domain() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.vallee.casa".into()], "c", "k")
            .await
            .unwrap();
        let hit = store.lookup_for_hostname("example.com").await;
        assert!(hit.is_none());
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        let store = WildcardCertStore::new();
        store
            .install(&["*.Vallee.Casa".into()], "c", "k")
            .await
            .unwrap();
        let hit = store.lookup_for_hostname("HOME.vallee.casa").await;
        assert!(hit.is_some());
    }
}
