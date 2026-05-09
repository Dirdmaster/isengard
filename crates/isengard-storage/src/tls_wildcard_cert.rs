//! Wildcard TLS cert PEM material. The existing `tls_cert` table is
//! metadata only (validity dates, serial) and FK-constrained to
//! `hosts(id)`, which doesn't fit the wildcard model where the cert
//! covers a domain not a single host.
//!
//! This is the source of truth for wildcard cert MATERIAL. The
//! controller hydrates its in-memory `WildcardCertStore` from this
//! table at boot; the ACME scheduler upserts a row after every
//! issuance / renewal.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Inventory, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WildcardCertRow {
    /// Canonical key, typically the wildcard form when the order covered both
    /// the wildcard and the apex (`*.foo.com`).
    pub primary_identifier: String,
    /// All SANs in the issued cert, in original order. JSON-encoded in SQLite.
    pub identifiers: Vec<String>,
    pub cert_pem: String,
    pub key_pem: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub serial: String,
    pub issuer: String,
}

#[derive(Debug, Clone)]
pub struct UpsertWildcardCert {
    pub primary_identifier: String,
    pub identifiers: Vec<String>,
    pub cert_pem: String,
    pub key_pem: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub serial: String,
    pub issuer: String,
}

impl Inventory {
    /// Insert or replace the wildcard cert keyed by `primary_identifier`.
    /// The `identifiers` list is JSON-encoded in the row.
    pub async fn upsert_wildcard_cert(&self, cert: UpsertWildcardCert) -> Result<()> {
        let identifiers_json =
            serde_json::to_string(&cert.identifiers).map_err(|e| crate::error::Error::Decode {
                reason: format!("serialize identifiers: {e}"),
            })?;
        sqlx::query(
            r#"
            INSERT INTO tls_wildcard_certs (
                primary_identifier, identifiers_json, cert_pem, key_pem,
                not_before, not_after, serial, issuer, updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
            ON CONFLICT(primary_identifier) DO UPDATE SET
                identifiers_json = excluded.identifiers_json,
                cert_pem         = excluded.cert_pem,
                key_pem          = excluded.key_pem,
                not_before       = excluded.not_before,
                not_after        = excluded.not_after,
                serial           = excluded.serial,
                issuer           = excluded.issuer,
                updated_at       = CURRENT_TIMESTAMP
            "#,
        )
        .bind(&cert.primary_identifier)
        .bind(&identifiers_json)
        .bind(&cert.cert_pem)
        .bind(&cert.key_pem)
        .bind(cert.not_before.to_rfc3339())
        .bind(cert.not_after.to_rfc3339())
        .bind(&cert.serial)
        .bind(&cert.issuer)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Snapshot every wildcard cert in storage. Called at controller boot to
    /// hydrate the in-memory `WildcardCertStore`.
    pub async fn list_wildcard_certs(&self) -> Result<Vec<WildcardCertRow>> {
        let rows = sqlx::query(
            r#"
            SELECT primary_identifier, identifiers_json, cert_pem, key_pem,
                   not_before, not_after, serial, issuer
            FROM tls_wildcard_certs
            ORDER BY primary_identifier
            "#,
        )
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let identifiers_json: String = row.try_get("identifiers_json")?;
            let identifiers: Vec<String> =
                serde_json::from_str(&identifiers_json).map_err(|e| {
                    crate::error::Error::Decode {
                        reason: format!("decode identifiers_json: {e}"),
                    }
                })?;
            let not_before_s: String = row.try_get("not_before")?;
            let not_after_s: String = row.try_get("not_after")?;
            let not_before = DateTime::parse_from_rfc3339(&not_before_s)
                .map_err(|e| crate::error::Error::Decode {
                    reason: format!("parse not_before {not_before_s}: {e}"),
                })?
                .with_timezone(&Utc);
            let not_after = DateTime::parse_from_rfc3339(&not_after_s)
                .map_err(|e| crate::error::Error::Decode {
                    reason: format!("parse not_after {not_after_s}: {e}"),
                })?
                .with_timezone(&Utc);
            out.push(WildcardCertRow {
                primary_identifier: row.try_get("primary_identifier")?,
                identifiers,
                cert_pem: row.try_get("cert_pem")?,
                key_pem: row.try_get("key_pem")?,
                not_before,
                not_after,
                serial: row.try_get("serial")?,
                issuer: row.try_get("issuer")?,
            });
        }
        Ok(out)
    }

    /// Delete a wildcard cert by primary identifier. Used by tests and
    /// future operator-driven revocation; not on the production hot path.
    pub async fn delete_wildcard_cert(&self, primary_identifier: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM tls_wildcard_certs WHERE primary_identifier = ?")
            .bind(primary_identifier)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(primary: &str) -> UpsertWildcardCert {
        UpsertWildcardCert {
            primary_identifier: primary.to_string(),
            identifiers: vec![
                primary.to_string(),
                primary.trim_start_matches("*.").to_string(),
            ],
            cert_pem: format!(
                "-----BEGIN CERTIFICATE-----\n{primary}\n-----END CERTIFICATE-----\n"
            ),
            key_pem: format!(
                "-----BEGIN PRIVATE KEY-----\n{primary}-key\n-----END PRIVATE KEY-----\n"
            ),
            not_before: Utc::now(),
            not_after: Utc::now() + chrono::Duration::days(90),
            serial: "ff:00".into(),
            issuer: "Let's Encrypt E7".into(),
        }
    }

    #[tokio::test]
    async fn upsert_then_list_round_trips() {
        let inv = Inventory::open_in_memory().await.unwrap();
        inv.upsert_wildcard_cert(sample("*.vallee.casa"))
            .await
            .unwrap();
        let rows = inv.list_wildcard_certs().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].primary_identifier, "*.vallee.casa");
        assert_eq!(
            rows[0].identifiers,
            vec!["*.vallee.casa".to_string(), "vallee.casa".to_string()]
        );
        assert!(rows[0].cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(rows[0].key_pem.contains("BEGIN PRIVATE KEY"));
    }

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let inv = Inventory::open_in_memory().await.unwrap();
        inv.upsert_wildcard_cert(sample("*.x.com")).await.unwrap();

        let mut second = sample("*.x.com");
        second.cert_pem = "-----BEGIN CERTIFICATE-----\nv2\n-----END CERTIFICATE-----\n".into();
        inv.upsert_wildcard_cert(second).await.unwrap();

        let rows = inv.list_wildcard_certs().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].cert_pem.contains("v2"));
    }

    #[tokio::test]
    async fn delete_returns_true_when_row_existed() {
        let inv = Inventory::open_in_memory().await.unwrap();
        inv.upsert_wildcard_cert(sample("*.foo.com")).await.unwrap();
        assert!(inv.delete_wildcard_cert("*.foo.com").await.unwrap());
        assert!(!inv.delete_wildcard_cert("*.foo.com").await.unwrap());
    }
}
