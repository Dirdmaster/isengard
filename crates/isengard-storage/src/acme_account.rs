//! ACME account singleton.
//!
//! One controller has one account. Migration `0010` creates the
//! `acme_account` table with `CHECK (id = 1)`, mirroring the [`ca`]
//! pattern. Holds the account key, the directory URL the key was
//! registered against, and the issuer-assigned `kid` (key
//! identifier) used on subsequent ACME requests.
//!
//! [`ca`]: crate::ca

use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row from `acme_account`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeAccount {
    /// Contact email registered with the ACME directory.
    pub contact_email: String,
    /// Directory URL the account is registered against
    /// (e.g. `https://acme-v02.api.letsencrypt.org/directory`).
    pub directory_url: String,
    /// Account key PEM. Never logged.
    pub account_key_pem: String,
    /// Server-assigned key identifier. `None` before the first
    /// successful `newAccount` round-trip.
    pub kid: Option<String>,
    /// When the row was first written.
    pub created_at: DateTime<Utc>,
    /// When the row was last replaced.
    pub updated_at: DateTime<Utc>,
}

/// Insert / replace payload for the ACME account row.
#[derive(Debug, Clone)]
pub struct UpsertAcmeAccount {
    /// Contact email to register.
    pub contact_email: String,
    /// Directory URL the key is registered against.
    pub directory_url: String,
    /// Account key PEM (caller mints it).
    pub account_key_pem: String,
    /// Server-assigned key id if known.
    pub kid: Option<String>,
}

impl crate::inventory::Inventory {
    /// Insert or replace the singleton ACME account row.
    ///
    /// `updated_at` bumps to `CURRENT_TIMESTAMP` on every call. The
    /// `CHECK (id = 1)` constraint guarantees there is exactly one
    /// row; the upsert keys on the constraint conflict.
    pub async fn upsert_acme_account(&self, ins: UpsertAcmeAccount) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO acme_account (id, contact_email, directory_url, account_key_pem, kid)
            VALUES (1, ?, ?, ?, ?)
            ON CONFLICT (id) DO UPDATE SET
              contact_email   = excluded.contact_email,
              directory_url   = excluded.directory_url,
              account_key_pem = excluded.account_key_pem,
              kid             = excluded.kid,
              updated_at      = CURRENT_TIMESTAMP
            "#,
        )
        .bind(&ins.contact_email)
        .bind(&ins.directory_url)
        .bind(&ins.account_key_pem)
        .bind(&ins.kid)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Fetch the singleton ACME account, or `None` when no account
    /// has been registered yet.
    pub async fn get_acme_account(&self) -> Result<Option<AcmeAccount>> {
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT contact_email, directory_url, account_key_pem, kid, created_at, updated_at \
             FROM acme_account WHERE id = 1",
        )
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else {
            return Ok(None);
        };

        let contact_email: String = r.try_get("contact_email")?;
        let directory_url: String = r.try_get("directory_url")?;
        let account_key_pem: String = r.try_get("account_key_pem")?;
        let kid: Option<String> = r.try_get("kid")?;
        let created_at = parse_sqlite_timestamp(r.try_get("created_at")?)?;
        let updated_at = parse_sqlite_timestamp(r.try_get("updated_at")?)?;

        Ok(Some(AcmeAccount {
            contact_email,
            directory_url,
            account_key_pem,
            kid,
            created_at,
            updated_at,
        }))
    }
}

/// Parse a timestamp string written by either RFC3339 (our explicit binds)
/// or SQLite's `CURRENT_TIMESTAMP` default ("YYYY-MM-DD HH:MM:SS").
fn parse_sqlite_timestamp(s: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s)
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                .map(|n| n.and_utc().fixed_offset())
        })
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| Error::Decode {
            reason: format!("bad timestamp '{s}': {e}"),
        })
}
