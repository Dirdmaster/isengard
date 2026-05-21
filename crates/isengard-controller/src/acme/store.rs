//! In-memory cache + SQLite-backed store for issued wildcard certs.
//!
//! SQLite is the source of truth: the in-memory `HashMap` is a hot-path
//! cache that gets hydrated at controller boot (`hydrate_from`) and
//! synced on every install. A controller restart loses the cache but
//! refills it from the persistent table on next boot, so the agent
//! receives the same cert material via the next `ProxyConfig` push.
//!
//! Keyed by primary identifier (e.g. `*.vallee.casa`). All other identifiers
//! covered by the same cert (the apex `vallee.casa`) are tracked alongside so
//! the lookup-by-hostname path can match either.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use isengard_storage::{Inventory, UpsertWildcardCert};

/// One issued wildcard cert in memory.
#[derive(Debug, Clone)]
pub struct WildcardCert {
    /// All SANs covered by this cert. The first is the primary key.
    pub identifiers: Vec<String>,
    /// PEM-encoded certificate chain.
    pub cert_pem: String,
    /// PEM-encoded leaf private key.
    pub key_pem: String,
}

/// In-memory cache keyed by primary identifier.
///
/// The on-disk `tls_wildcard_certs` table is the source of truth;
/// this cache hydrates from there at boot.
#[derive(Debug, Default)]
pub struct WildcardCertStore {
    /// `primary_identifier` -> shared cert handle.
    inner: RwLock<HashMap<String, Arc<WildcardCert>>>,
}

impl WildcardCertStore {
    /// Builds an empty store. Hydrate from storage via
    /// [`WildcardCertStore::hydrate_from`] before the first agent
    /// push.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hydrate the in-memory cache from SQLite. Called at controller boot
    /// before the renewal scheduler ticks so a restart doesn't strand the
    /// agent with no cert material until a fresh ACME issuance.
    pub async fn hydrate_from(&self, inv: &Inventory) -> Result<usize> {
        let rows = inv
            .list_wildcard_certs()
            .await
            .context("hydrate wildcard certs from storage")?;
        let count = rows.len();
        let mut guard = self.inner.write().await;
        guard.clear();
        for row in rows {
            let entry = Arc::new(WildcardCert {
                identifiers: row.identifiers,
                cert_pem: row.cert_pem,
                key_pem: row.key_pem,
            });
            guard.insert(row.primary_identifier, entry);
        }
        Ok(count)
    }

    /// Install a freshly-issued cert into the in-memory cache. Replaces any
    /// existing cert under the same primary identifier. Storage persistence
    /// happens separately (the scheduler calls
    /// `Inventory::upsert_wildcard_cert` alongside this so a single boot
    /// after the issuance is enough to survive a restart even if the cert
    /// was just minted).
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

    /// Persist the cert material to SQLite. Companion to `install` for the
    /// cases where the caller has the parsed validity window. Idempotent:
    /// upserts on `primary_identifier`.
    #[allow(clippy::too_many_arguments)]
    pub async fn persist(
        &self,
        inv: &Inventory,
        identifiers: &[String],
        cert_pem: &str,
        key_pem: &str,
        not_before: DateTime<Utc>,
        not_after: DateTime<Utc>,
        serial: &str,
        issuer: &str,
    ) -> Result<()> {
        if identifiers.is_empty() {
            return Err(anyhow::anyhow!("persist: identifiers must be non-empty"));
        }
        inv.upsert_wildcard_cert(UpsertWildcardCert {
            primary_identifier: identifiers[0].clone(),
            identifiers: identifiers.to_vec(),
            cert_pem: cert_pem.to_string(),
            key_pem: key_pem.to_string(),
            not_before,
            not_after,
            serial: serial.to_string(),
            issuer: issuer.to_string(),
        })
        .await
        .context("upsert wildcard cert")
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
