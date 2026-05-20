//! TLS certificate metadata.
//!
//! Migration `0010` lands `tls_certs`. The actual PEM cert+key live on
//! the agent's filesystem at `/var/lib/isengard/tls/<hostname>.{crt,key}`.
//! This table tracks which certs the controller knows about and when
//! to renew them.
//!
//! The renewal scheduler reads via `Inventory::list_tls_certs_due`
//! once an hour; success and failure outcomes flow through
//! `Inventory::upsert_tls_cert_meta` and
//! `Inventory::record_tls_attempt`.

use crate::error::{Error, Result};
use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row from `tls_certs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsCertMeta {
    /// Hostname the cert covers (the lookup key).
    pub public_hostname: String,
    /// Host that holds the on-disk PEM.
    pub host_id: HostId,
    /// Issuer string (CN or common name).
    pub issuer: String,
    /// Validity start.
    pub not_before: DateTime<Utc>,
    /// Validity end.
    pub not_after: DateTime<Utc>,
    /// Last successful renewal timestamp.
    pub last_renewed_at: Option<DateTime<Utc>>,
    /// When the renewal scheduler should next try.
    pub next_renewal_at: DateTime<Utc>,
    /// X.509 serial (hex), when known.
    pub serial: Option<String>,
    /// Timestamp of the most recent attempt (success or failure).
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// Last failure reason string.
    pub last_error: Option<String>,
    /// Consecutive failure counter. Resets to 0 on success.
    pub attempt_count: u32,
}

/// Upsert payload for `tls_certs`.
#[derive(Debug, Clone)]
pub struct UpsertTlsCertMeta {
    /// Hostname the cert covers.
    pub public_hostname: String,
    /// Host that holds the on-disk PEM.
    pub host_id: HostId,
    /// Issuer string.
    pub issuer: String,
    /// Validity start.
    pub not_before: DateTime<Utc>,
    /// Validity end.
    pub not_after: DateTime<Utc>,
    /// Computed renewal target.
    pub next_renewal_at: DateTime<Utc>,
    /// X.509 serial (hex), when known.
    pub serial: Option<String>,
}

impl crate::inventory::Inventory {
    /// Upsert the metadata for a successfully-issued cert.
    ///
    /// Sets `last_renewed_at = CURRENT_TIMESTAMP` on both INSERT and
    /// UPDATE: calling this means we got a fresh cert from the
    /// issuer, so "renewed at" is now in either case.
    ///
    /// Caller contract: this method does NOT touch `attempt_count` or
    /// `last_error`. After a successful issuance, also call
    /// `Inventory::record_tls_attempt` with `success = true` so the
    /// attempt counter and error string reflect the success. The split
    /// is intentional: the renewal scheduler can record an attempt
    /// without a fresh cert (failure path) and vice versa.
    pub async fn upsert_tls_cert_meta(&self, ins: UpsertTlsCertMeta) -> Result<()> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let nb = ins.not_before.to_rfc3339();
        let na = ins.not_after.to_rfc3339();
        let nr = ins.next_renewal_at.to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO tls_certs (
              public_hostname, host_id, issuer, not_before, not_after,
              next_renewal_at, last_renewed_at, serial
            ) VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, ?)
            ON CONFLICT (public_hostname) DO UPDATE SET
              host_id = excluded.host_id,
              issuer = excluded.issuer,
              not_before = excluded.not_before,
              not_after = excluded.not_after,
              next_renewal_at = excluded.next_renewal_at,
              last_renewed_at = CURRENT_TIMESTAMP,
              serial = excluded.serial
            "#,
        )
        .bind(&ins.public_hostname)
        .bind(&host_bytes)
        .bind(&ins.issuer)
        .bind(&nb)
        .bind(&na)
        .bind(&nr)
        .bind(&ins.serial)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Fetch the cert metadata for `public_hostname`.
    pub async fn get_tls_cert_meta(&self, public_hostname: &str) -> Result<Option<TlsCertMeta>> {
        let row = sqlx::query(
            "SELECT public_hostname, host_id, issuer, not_before, not_after, \
             last_renewed_at, next_renewal_at, serial, \
             last_attempt_at, last_error, attempt_count \
             FROM tls_certs WHERE public_hostname = ?",
        )
        .bind(public_hostname)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(r) => Ok(Some(decode_tls_cert_row(r)?)),
            None => Ok(None),
        }
    }

    /// Every cert whose `next_renewal_at` is at or before `before`.
    ///
    /// The renewal scheduler calls this once an hour with `Utc::now()`
    /// to find certs due for re-issuance.
    pub async fn list_tls_certs_due(&self, before: DateTime<Utc>) -> Result<Vec<TlsCertMeta>> {
        let cutoff = before.to_rfc3339();
        let rows = sqlx::query(
            r#"
            SELECT public_hostname, host_id, issuer, not_before, not_after,
                   last_renewed_at, next_renewal_at, serial,
                   last_attempt_at, last_error, attempt_count
            FROM tls_certs
            WHERE next_renewal_at <= ?
            "#,
        )
        .bind(&cutoff)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(decode_tls_cert_row).collect()
    }

    /// Record the outcome of a renewal attempt.
    ///
    /// On success, `attempt_count` resets to 0. The field tracks
    /// CONSECUTIVE FAILURES (analogous to Plan A's healthcheck
    /// eviction counter), not "total attempts ever". Without this
    /// reset, the backoff window in `tls/renewal.rs::should_retry`
    /// continues to grow even after a successful renewal, which means
    /// a long-lived agent eventually hits the 24h cap and won't
    /// renew on time.
    pub async fn record_tls_attempt(
        &self,
        public_hostname: &str,
        success: bool,
        error: Option<String>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        if success {
            sqlx::query(
                "UPDATE tls_certs SET last_attempt_at = ?, last_error = NULL, \
                 attempt_count = 0 WHERE public_hostname = ?",
            )
            .bind(&now)
            .bind(public_hostname)
            .execute(self.pool())
            .await?;
        } else {
            sqlx::query(
                "UPDATE tls_certs SET last_attempt_at = ?, last_error = ?, \
                 attempt_count = attempt_count + 1 WHERE public_hostname = ?",
            )
            .bind(&now)
            .bind(&error)
            .bind(public_hostname)
            .execute(self.pool())
            .await?;
        }
        Ok(())
    }
}

/// Parse a timestamp written either explicitly as RFC3339 (our
/// `to_rfc3339()` binds) or implicitly by SQLite's `CURRENT_TIMESTAMP`
/// default (which uses `"YYYY-MM-DD HH:MM:SS"`).
fn parse_db_timestamp(s: &str, field: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .map(|n| n.and_utc().fixed_offset())
        })
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| Error::Decode {
            reason: format!("invalid {field} '{s}': {e}"),
        })
}

/// Decode one `tls_certs` row. Shared by the get + list paths.
fn decode_tls_cert_row(r: sqlx::sqlite::SqliteRow) -> Result<TlsCertMeta> {
    use sqlx::Row;

    let public_hostname: String = r.try_get("public_hostname")?;
    let host_bytes: Vec<u8> = r.try_get("host_id")?;
    let host_id = HostId::from_db_bytes(host_bytes)?;
    let issuer: String = r.try_get("issuer")?;
    let not_before_str: String = r.try_get("not_before")?;
    let not_after_str: String = r.try_get("not_after")?;
    let last_renewed_at_str: Option<String> = r.try_get("last_renewed_at")?;
    let next_renewal_at_str: String = r.try_get("next_renewal_at")?;
    let serial: Option<String> = r.try_get("serial")?;
    let last_attempt_at_str: Option<String> = r.try_get("last_attempt_at")?;
    let last_error: Option<String> = r.try_get("last_error")?;
    let attempt_count_i64: i64 = r.try_get("attempt_count")?;
    let attempt_count: u32 = attempt_count_i64.try_into().map_err(|_| Error::Decode {
        reason: format!("attempt_count out of range: {attempt_count_i64}"),
    })?;

    let not_before = parse_db_timestamp(&not_before_str, "not_before")?;
    let not_after = parse_db_timestamp(&not_after_str, "not_after")?;
    let next_renewal_at = parse_db_timestamp(&next_renewal_at_str, "next_renewal_at")?;
    let last_renewed_at = match last_renewed_at_str {
        Some(s) => Some(parse_db_timestamp(&s, "last_renewed_at")?),
        None => None,
    };
    let last_attempt_at = match last_attempt_at_str {
        Some(s) => Some(parse_db_timestamp(&s, "last_attempt_at")?),
        None => None,
    };

    Ok(TlsCertMeta {
        public_hostname,
        host_id,
        issuer,
        not_before,
        not_after,
        last_renewed_at,
        next_renewal_at,
        serial,
        last_attempt_at,
        last_error,
        attempt_count,
    })
}
