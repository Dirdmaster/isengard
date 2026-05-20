//! Per-agent leaf certificates issued by the controller's CA.
//!
//! Migration `0014` lands `agent_certs`. Each row holds the issued PEM
//! (no key: the agent generates the keypair locally and only sends
//! the CSR), validity bounds, and revocation state. The `serial` is
//! the X.509 serial (binary) and is the primary key.
//!
//! Revocation is split into two paths: [`Inventory::revoke_cert`]
//! drops one serial, [`Inventory::revoke_all_active_for_host`] kicks
//! a host off entirely (every active cert at once, returns the
//! serials it touched).

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::error::Error;
use crate::host::HostId;
use crate::{Inventory, Result};

/// One row from `agent_certs`.
#[derive(Debug, Clone)]
pub struct AgentCert {
    /// X.509 serial (binary). Primary key.
    pub serial: Vec<u8>,
    /// Host this leaf was minted for.
    pub host_id: HostId,
    /// Issued PEM.
    pub cert_pem: String,
    /// Validity start.
    pub issued_at: DateTime<Utc>,
    /// Validity end.
    pub expires_at: DateTime<Utc>,
    /// `Some(ts)` when the cert has been revoked.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Operator-readable reason set at revocation time.
    pub revoke_reason: Option<String>,
}

impl Inventory {
    /// Insert a freshly-signed agent leaf cert.
    ///
    /// Caller supplies all fields; `revoked_at` and `revoke_reason`
    /// are expected to be `None` at issuance.
    pub async fn insert_agent_cert(&self, cert: AgentCert) -> Result<()> {
        let host_bytes = cert.host_id.to_bytes().to_vec();
        let issued = cert.issued_at.to_rfc3339();
        let expires = cert.expires_at.to_rfc3339();
        sqlx::query(
            "INSERT INTO agent_certs (serial, host_id, cert_pem, issued_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&cert.serial)
        .bind(&host_bytes)
        .bind(&cert.cert_pem)
        .bind(&issued)
        .bind(&expires)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The most recently-issued non-revoked cert for `host_id`.
    ///
    /// Returns `None` if the host has no active cert. Used by the
    /// dashboard to show per-host cert status and by the renewal task
    /// to detect "current" cert.
    pub async fn active_cert_for_host(&self, host_id: HostId) -> Result<Option<AgentCert>> {
        let host_bytes = host_id.to_bytes().to_vec();
        let row = sqlx::query(
            "SELECT serial, host_id, cert_pem, issued_at, expires_at, revoked_at, revoke_reason
             FROM agent_certs
             WHERE host_id = ? AND revoked_at IS NULL
             ORDER BY issued_at DESC
             LIMIT 1",
        )
        .bind(&host_bytes)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(r) => Ok(Some(decode_agent_cert_row(r)?)),
            None => Ok(None),
        }
    }

    /// Mark a single cert as revoked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] if the serial doesn't exist or the
    /// cert was already revoked (rows-affected == 0).
    pub async fn revoke_cert(&self, serial: &[u8], reason: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE agent_certs SET revoked_at = ?, revoke_reason = ?
             WHERE serial = ? AND revoked_at IS NULL",
        )
        .bind(&now)
        .bind(reason)
        .bind(serial)
        .execute(self.pool())
        .await?;

        if res.rows_affected() == 0 {
            return Err(Error::Conflict("cert not found or already revoked".into()));
        }
        Ok(())
    }

    /// Atomically revoke every currently-active cert for `host_id`.
    ///
    /// Returns the serials that were revoked. Pre-fix the
    /// controller's `revoke_agent` only revoked the most-recently-issued
    /// active cert; after a renewal the older active cert kept working
    /// until separately revoked. The fix moves the "kick this host off"
    /// semantics from "newest cert" to "every active cert" (Imp-3).
    ///
    /// Returns an empty `Vec` if the host has no active certs (already
    /// revoked or never had one). That case is not an error so callers
    /// can use the same code path for "revoke whatever's there"
    /// without checking first.
    pub async fn revoke_all_active_for_host(
        &self,
        host_id: HostId,
        reason: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let host_bytes = host_id.to_bytes().to_vec();
        // Read first so we can return the affected serials. UPDATE then RETURNING
        // would be ideal, but sqlx + sqlite migrations don't expose RETURNING
        // through the macros we use here, so a tiny SELECT-then-UPDATE inside
        // a single connection pass is fine.
        let serials: Vec<Vec<u8>> =
            sqlx::query("SELECT serial FROM agent_certs WHERE host_id = ? AND revoked_at IS NULL")
                .bind(&host_bytes)
                .fetch_all(self.pool())
                .await?
                .into_iter()
                .map(|r| r.get::<Vec<u8>, _>(0))
                .collect();

        if serials.is_empty() {
            return Ok(serials);
        }

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE agent_certs SET revoked_at = ?, revoke_reason = ?
             WHERE host_id = ? AND revoked_at IS NULL",
        )
        .bind(&now)
        .bind(reason)
        .bind(&host_bytes)
        .execute(self.pool())
        .await?;

        Ok(serials)
    }

    /// Every revoked serial.
    ///
    /// The interceptor uses this to populate its in-memory revocation
    /// set on startup; mutations after startup are pushed via
    /// [`Inventory::revoke_cert`] plus an in-process notify channel
    /// (handled at the controller layer).
    pub async fn revoked_serials(&self) -> Result<Vec<Vec<u8>>> {
        let rows = sqlx::query("SELECT serial FROM agent_certs WHERE revoked_at IS NOT NULL")
            .fetch_all(self.pool())
            .await?;

        Ok(rows.into_iter().map(|r| r.get::<Vec<u8>, _>(0)).collect())
    }
}

/// Decode one `agent_certs` row into an [`AgentCert`].
fn decode_agent_cert_row(r: sqlx::sqlite::SqliteRow) -> Result<AgentCert> {
    let serial: Vec<u8> = r.get(0);
    let host_bytes: Vec<u8> = r.get(1);
    let host_id = HostId::from_db_bytes(host_bytes)?;
    let cert_pem: String = r.get(2);
    let issued_at_str: String = r.get(3);
    let expires_at_str: String = r.get(4);
    let revoked_at_str: Option<String> = r.get(5);
    let revoke_reason: Option<String> = r.get(6);

    let issued_at = parse_db_timestamp(&issued_at_str, "issued_at")?;
    let expires_at = parse_db_timestamp(&expires_at_str, "expires_at")?;
    let revoked_at = match revoked_at_str {
        Some(s) => Some(parse_db_timestamp(&s, "revoked_at")?),
        None => None,
    };

    Ok(AgentCert {
        serial,
        host_id,
        cert_pem,
        issued_at,
        expires_at,
        revoked_at,
        revoke_reason,
    })
}

/// Parse an RFC3339 or SQLite `CURRENT_TIMESTAMP` string for column
/// `field`. Returns [`Error::Decode`] naming the field on failure.
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
