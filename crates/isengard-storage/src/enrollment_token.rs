//! Short-lived enrollment tokens.
//!
//! Migration `0014` lands the original `enrollment_tokens` table;
//! `0015` adds the `cancelled_at` column for Imp-4. The plaintext
//! token is shown to the operator exactly once at mint time; only
//! its SHA-256 hash is persisted. Tokens carry a role (currently
//! always [`TokenRole::Agent`]) and an expiry; consumption is
//! atomic via a conditional `UPDATE`.

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::error::Error;
use crate::host::HostId;
use crate::{Inventory, Result};

/// Role attached to a minted enrollment token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRole {
    /// Token mints an agent leaf cert when redeemed.
    Agent,
}

impl TokenRole {
    /// Canonical string used for the `role` TEXT column.
    fn as_str(self) -> &'static str {
        match self {
            TokenRole::Agent => "agent",
        }
    }

    /// Parse a role TEXT column.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] for unknown values; this guards against
    /// an older or newer agent writing a role this binary cannot honour.
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "agent" => Ok(TokenRole::Agent),
            other => Err(Error::Decode {
                reason: format!("unknown enrollment token role '{other}'"),
            }),
        }
    }
}

/// One row from `enrollment_tokens`.
#[derive(Debug, Clone)]
pub struct EnrollmentTokenRecord {
    /// SHA-256 hash of the plaintext token.
    pub token_hash: Vec<u8>,
    /// Role the token mints.
    pub role: TokenRole,
    /// When the token stops being usable.
    pub expires_at: DateTime<Utc>,
    /// `Some(ts)` when a host redeemed the token.
    pub consumed_at: Option<DateTime<Utc>>,
    /// `Some(host)` when a host redeemed the token.
    pub consumed_by: Option<HostId>,
    /// Imp-4 fix: distinct from `consumed_at`. Cancellation marks the
    /// token unusable without falsely claiming a host redeemed it.
    /// Filled by [`Inventory::cancel_enrollment_token`].
    pub cancelled_at: Option<DateTime<Utc>>,
    /// When the row was inserted.
    pub created_at: DateTime<Utc>,
}

impl Inventory {
    /// Insert a new enrollment token row keyed by its SHA-256
    /// `token_hash`.
    pub async fn insert_enrollment_token(
        &self,
        token_hash: Vec<u8>,
        role: TokenRole,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let exp = expires_at.to_rfc3339();
        sqlx::query(
            "INSERT INTO enrollment_tokens (token_hash, role, expires_at) VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(role.as_str())
        .bind(&exp)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Look up a token by hash.
    ///
    /// Returns `None` if the token doesn't exist, has been consumed,
    /// has been cancelled (Imp-4), or has expired.
    pub async fn find_active_token(&self, hash: &[u8]) -> Result<Option<EnrollmentTokenRecord>> {
        let now = Utc::now().to_rfc3339();
        let row = sqlx::query(
            "SELECT token_hash, role, expires_at, consumed_at, consumed_by, cancelled_at, created_at
             FROM enrollment_tokens
             WHERE token_hash = ? AND consumed_at IS NULL AND cancelled_at IS NULL AND expires_at > ?",
        )
        .bind(hash)
        .bind(&now)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(r) => Ok(Some(decode_token_row(r)?)),
            None => Ok(None),
        }
    }

    /// Atomically consume a token by binding it to `host_id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] if the token is missing or already
    /// consumed. Cancelled tokens are also rejected: once an operator
    /// pulls the rug on an invitation, no enrollment can revive it.
    pub async fn consume_enrollment_token(&self, hash: &[u8], host_id: HostId) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let host_bytes = host_id.to_bytes().to_vec();
        let res = sqlx::query(
            "UPDATE enrollment_tokens
             SET consumed_at = ?, consumed_by = ?
             WHERE token_hash = ? AND consumed_at IS NULL AND cancelled_at IS NULL",
        )
        .bind(&now)
        .bind(&host_bytes)
        .bind(hash)
        .execute(self.pool())
        .await?;

        if res.rows_affected() == 0 {
            return Err(Error::Conflict(
                "enrollment token not found, already consumed, or cancelled".into(),
            ));
        }
        Ok(())
    }

    /// Mark an unused token as cancelled (Imp-4).
    ///
    /// Distinct from [`Self::consume_enrollment_token`] so the audit
    /// trail doesn't pretend a sentinel `HostId` enrolled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] if the token is missing, already
    /// consumed, or already cancelled.
    pub async fn cancel_enrollment_token(&self, hash: &[u8]) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE enrollment_tokens
             SET cancelled_at = ?
             WHERE token_hash = ? AND consumed_at IS NULL AND cancelled_at IS NULL",
        )
        .bind(&now)
        .bind(hash)
        .execute(self.pool())
        .await?;

        if res.rows_affected() == 0 {
            return Err(Error::Conflict(
                "enrollment token not found, already consumed, or already cancelled".into(),
            ));
        }
        Ok(())
    }

    /// List all currently-active (unexpired, unconsumed, uncancelled)
    /// enrollment tokens, most recently created first.
    ///
    /// Used by the dashboard to show pending invitations.
    pub async fn list_active_tokens(&self) -> Result<Vec<EnrollmentTokenRecord>> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT token_hash, role, expires_at, consumed_at, consumed_by, cancelled_at, created_at
             FROM enrollment_tokens
             WHERE consumed_at IS NULL AND cancelled_at IS NULL AND expires_at > ?
             ORDER BY created_at DESC",
        )
        .bind(&now)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(decode_token_row).collect()
    }
}

/// Decode one `enrollment_tokens` row into an [`EnrollmentTokenRecord`].
fn decode_token_row(r: sqlx::sqlite::SqliteRow) -> Result<EnrollmentTokenRecord> {
    let token_hash: Vec<u8> = r.get(0);
    let role_str: String = r.get(1);
    let role = TokenRole::from_str(&role_str)?;
    let expires_at_str: String = r.get(2);
    let consumed_at_str: Option<String> = r.get(3);
    let consumed_by_bytes: Option<Vec<u8>> = r.get(4);
    let cancelled_at_str: Option<String> = r.get(5);
    let created_at_str: String = r.get(6);

    let expires_at = parse_db_timestamp(&expires_at_str, "expires_at")?;
    let consumed_at = match consumed_at_str {
        Some(s) => Some(parse_db_timestamp(&s, "consumed_at")?),
        None => None,
    };
    let cancelled_at = match cancelled_at_str {
        Some(s) => Some(parse_db_timestamp(&s, "cancelled_at")?),
        None => None,
    };
    let created_at = parse_db_timestamp(&created_at_str, "created_at")?;
    let consumed_by = match consumed_by_bytes {
        Some(b) => Some(HostId::from_db_bytes(b)?),
        None => None,
    };

    Ok(EnrollmentTokenRecord {
        token_hash,
        role,
        expires_at,
        consumed_at,
        consumed_by,
        cancelled_at,
        created_at,
    })
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
