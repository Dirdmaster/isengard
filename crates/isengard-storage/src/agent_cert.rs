//! Per-agent leaf certificates issued by the controller's CA. Each row holds
//! the issued PEM (no key — the agent generates the keypair locally and only
//! sends the CSR), validity bounds, and revocation state. The `serial` is
//! the X.509 serial (binary) and serves as the primary key.

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::error::Error;
use crate::host::HostId;
use crate::{Inventory, Result};

#[derive(Debug, Clone)]
pub struct AgentCert {
    pub serial: Vec<u8>,
    pub host_id: HostId,
    pub cert_pem: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoke_reason: Option<String>,
}

impl Inventory {
    /// Insert a freshly-signed agent leaf cert. Caller supplies all fields;
    /// `revoked_at` and `revoke_reason` are expected to be `None` at issuance.
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

    /// Return the most recently-issued non-revoked cert for `host_id`, or
    /// `None` if the host has no active cert. Used by the dashboard to show
    /// per-host cert status and by the renewal task to detect "current" cert.
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

    /// Mark a cert as revoked. Errors if the serial doesn't exist or the cert
    /// was already revoked (rows-affected == 0).
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

    /// Return the serials of every revoked cert. The interceptor uses this
    /// to populate its in-memory revocation set on startup; mutations after
    /// startup are pushed via [`Inventory::revoke_cert`] + an in-process
    /// notify channel (handled at the controller layer).
    pub async fn revoked_serials(&self) -> Result<Vec<Vec<u8>>> {
        let rows = sqlx::query("SELECT serial FROM agent_certs WHERE revoked_at IS NOT NULL")
            .fetch_all(self.pool())
            .await?;

        Ok(rows.into_iter().map(|r| r.get::<Vec<u8>, _>(0)).collect())
    }
}

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
