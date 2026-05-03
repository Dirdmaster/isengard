//! TLS certificate metadata. The actual PEM cert+key live on the agent's
//! filesystem at `/var/lib/isengard/tls/<hostname>.{crt,key}`. This table
//! tracks which certs we have and when to renew them.

use crate::error::{Error, Result};
use crate::host::HostId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsCertMeta {
    pub public_hostname: String,
    pub host_id: HostId,
    pub issuer: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub last_renewed_at: Option<DateTime<Utc>>,
    pub next_renewal_at: DateTime<Utc>,
    pub serial: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub attempt_count: u32,
}

#[derive(Debug, Clone)]
pub struct UpsertTlsCertMeta {
    pub public_hostname: String,
    pub host_id: HostId,
    pub issuer: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub next_renewal_at: DateTime<Utc>,
    pub serial: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn upsert_tls_cert_meta(&self, ins: UpsertTlsCertMeta) -> Result<()> {
        let host_bytes = ins.host_id.to_bytes().to_vec();
        let nb = ins.not_before.to_rfc3339();
        let na = ins.not_after.to_rfc3339();
        let nr = ins.next_renewal_at.to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO tls_certs (
              public_hostname, host_id, issuer, not_before, not_after,
              next_renewal_at, serial
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
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

    pub async fn get_tls_cert_meta(&self, public_hostname: &str) -> Result<Option<TlsCertMeta>> {
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT public_hostname, host_id, issuer, not_before, not_after, \
             last_renewed_at, next_renewal_at, serial, \
             last_attempt_at, last_error, attempt_count \
             FROM tls_certs WHERE public_hostname = ?",
        )
        .bind(public_hostname)
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else {
            return Ok(None);
        };

        let public_hostname: String = r.try_get("public_hostname")?;
        let host_bytes: Vec<u8> = r.try_get("host_id")?;
        let host_id = HostId::from_bytes(host_bytes.try_into().map_err(|_| Error::Decode {
            reason: "bad host_id length".into(),
        })?);
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

        let not_before = DateTime::parse_from_rfc3339(&not_before_str)
            .map_err(|e| Error::Decode {
                reason: format!("invalid not_before: {e}"),
            })?
            .with_timezone(&Utc);
        let not_after = DateTime::parse_from_rfc3339(&not_after_str)
            .map_err(|e| Error::Decode {
                reason: format!("invalid not_after: {e}"),
            })?
            .with_timezone(&Utc);
        let next_renewal_at = DateTime::parse_from_rfc3339(&next_renewal_at_str)
            .map_err(|e| Error::Decode {
                reason: format!("invalid next_renewal_at: {e}"),
            })?
            .with_timezone(&Utc);
        let last_renewed_at = match last_renewed_at_str {
            Some(s) => Some(
                DateTime::parse_from_rfc3339(&s)
                    .map_err(|e| Error::Decode {
                        reason: format!("invalid last_renewed_at: {e}"),
                    })?
                    .with_timezone(&Utc),
            ),
            None => None,
        };
        let last_attempt_at = match last_attempt_at_str {
            Some(s) => Some(
                DateTime::parse_from_rfc3339(&s)
                    .map_err(|e| Error::Decode {
                        reason: format!("invalid last_attempt_at: {e}"),
                    })?
                    .with_timezone(&Utc),
            ),
            None => None,
        };

        Ok(Some(TlsCertMeta {
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
        }))
    }

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
                 attempt_count = attempt_count + 1 WHERE public_hostname = ?",
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
