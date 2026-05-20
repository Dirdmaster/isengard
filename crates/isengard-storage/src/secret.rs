//! Isengard-managed secrets store.
//!
//! Migration `0025` lands the `secrets` table. Rows hold a
//! ChaCha20-Poly1305 ciphertext for an operator-supplied value; the
//! controller reads the 32-byte master key from a bind-mounted file
//! at boot (`ISENGARD_MASTER_KEY_FILE`, default
//! `/run/secrets/master.key`) and never persists it.
//!
//! The DAO trades in raw ciphertext bytes. The encryption layer lives
//! one level up (controller crate) so the storage crate stays free of
//! crypto deps. Callers encrypt before [`Inventory::upsert_secret`]
//! and decrypt after [`Inventory::get_secret_ciphertext`].
//!
//! Naming rules (enforced at the API boundary, not the DB): names are
//! `[a-zA-Z0-9._-]{1,64}`. The DB only enforces non-empty and uniqueness.
//!
//! Plaintext is never stored, never logged, never returned through any
//! list endpoint. [`SecretMeta`] intentionally omits the ciphertext
//! field; the only call that exposes ciphertext is
//! [`Inventory::get_secret_ciphertext`], used by the controller's
//! decrypt path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::Error;
use crate::{Inventory, Result};

/// Public-safe metadata for a stored secret.
///
/// The plaintext value is never exposed through this struct: list
/// endpoints serialize this directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMeta {
    /// Secret name (the lookup key).
    pub name: String,
    /// When the secret was first inserted.
    pub created_at: DateTime<Utc>,
    /// When the ciphertext was last rotated.
    pub updated_at: DateTime<Utc>,
    /// Operator identity that created the row, when known.
    pub created_by: Option<String>,
}

impl Inventory {
    /// Insert or replace a secret's ciphertext.
    ///
    /// Caller is responsible for encrypting `ciphertext` before calling.
    /// Returns whether a new row was inserted (`true`) or an existing
    /// row was replaced (`false`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if `name` fails
    /// [`validate_secret_name`].
    pub async fn upsert_secret(
        &self,
        name: &str,
        ciphertext: &[u8],
        created_by: Option<&str>,
    ) -> Result<bool> {
        validate_secret_name(name)?;

        // SQLite UPSERT with conditional update of `updated_at`. We can
        // detect insert-vs-update by checking row count after a
        // `INSERT ... ON CONFLICT DO UPDATE`: rows_affected() returns 1
        // for both, so we look up the row first to know.
        let existed: bool =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM secrets WHERE name = ?")
                .bind(name)
                .fetch_one(self.pool())
                .await?
                > 0;

        let now = Utc::now().to_rfc3339();
        if existed {
            sqlx::query("UPDATE secrets SET ciphertext = ?, updated_at = ? WHERE name = ?")
                .bind(ciphertext)
                .bind(&now)
                .bind(name)
                .execute(self.pool())
                .await?;
        } else {
            sqlx::query(
                "INSERT INTO secrets (name, ciphertext, created_at, updated_at, created_by)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(name)
            .bind(ciphertext)
            .bind(&now)
            .bind(&now)
            .bind(created_by)
            .execute(self.pool())
            .await?;
        }
        Ok(!existed)
    }

    /// Insert a secret, refusing to overwrite.
    ///
    /// Used by the `POST /api/v1/secrets` handler so the operator
    /// gets an explicit 409 instead of a silent overwrite.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] if the name is already present.
    /// Returns [`Error::Decode`] if `name` fails
    /// [`validate_secret_name`].
    pub async fn insert_secret_strict(
        &self,
        name: &str,
        ciphertext: &[u8],
        created_by: Option<&str>,
    ) -> Result<()> {
        validate_secret_name(name)?;
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO secrets (name, ciphertext, created_at, updated_at, created_by)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(name)
        .bind(ciphertext)
        .bind(&now)
        .bind(&now)
        .bind(created_by)
        .execute(self.pool())
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::Conflict(format!(
                "secret {name:?} already exists; PUT to replace"
            )));
        }
        Ok(())
    }

    /// Fetch the raw ciphertext for `name`.
    ///
    /// Returns `None` if no such secret exists. Caller is expected to
    /// decrypt with the controller's master key. Plaintext is never
    /// logged here.
    pub async fn get_secret_ciphertext(&self, name: &str) -> Result<Option<Vec<u8>>> {
        validate_secret_name(name)?;
        let row: Option<sqlx::sqlite::SqliteRow> =
            sqlx::query("SELECT ciphertext FROM secrets WHERE name = ?")
                .bind(name)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>(0)))
    }

    /// Fetch metadata (no ciphertext) for `name`.
    ///
    /// Used by the GET-by-id endpoint that answers "this secret
    /// exists, here's when it was set".
    pub async fn get_secret_meta(&self, name: &str) -> Result<Option<SecretMeta>> {
        validate_secret_name(name)?;
        let row: Option<sqlx::sqlite::SqliteRow> = sqlx::query(
            "SELECT name, created_at, updated_at, created_by FROM secrets WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(self.pool())
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(decode_meta_row(r)?)),
        }
    }

    /// List every stored secret, name plus timestamps.
    ///
    /// Plaintext is never returned through this surface; the
    /// `ciphertext` column is not even selected.
    pub async fn list_secrets(&self) -> Result<Vec<SecretMeta>> {
        let rows = sqlx::query(
            "SELECT name, created_at, updated_at, created_by
             FROM secrets ORDER BY name ASC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(decode_meta_row).collect()
    }

    /// Delete a secret. Returns `true` if a row was removed.
    pub async fn delete_secret(&self, name: &str) -> Result<bool> {
        validate_secret_name(name)?;
        let res = sqlx::query("DELETE FROM secrets WHERE name = ?")
            .bind(name)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Quick predicate. Retained for symmetry with the dashboard but no
    /// longer used as a boot gate: the controller refuses to start
    /// if the master key file is missing, regardless of whether the DB
    /// has stored secrets.
    pub async fn has_any_secret(&self) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets")
            .fetch_one(self.pool())
            .await?;
        Ok(n > 0)
    }
}

/// Decode one `secrets` row into a [`SecretMeta`] (no ciphertext).
fn decode_meta_row(r: sqlx::sqlite::SqliteRow) -> Result<SecretMeta> {
    let name: String = r.get(0);
    let created_at: String = r.get(1);
    let updated_at: String = r.get(2);
    let created_by: Option<String> = r.get(3);
    Ok(SecretMeta {
        name,
        created_at: parse_db_timestamp(&created_at, "created_at")?,
        updated_at: parse_db_timestamp(&updated_at, "updated_at")?,
        created_by,
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
            reason: format!("invalid {field} {s:?}: {e}"),
        })
}

/// Validate a secret name against the project rules.
///
/// Names must be non-empty, at most 64 chars, and match
/// `[A-Za-z0-9._-]+`. The pattern is what compose files allow for
/// `secrets:` keys and lets the controller safely substitute names
/// into file paths (e.g. `/run/secrets/<name>`).
///
/// # Errors
///
/// Returns [`Error::Decode`] with a reason naming the violation.
pub fn validate_secret_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Decode {
            reason: "secret name must be non-empty".into(),
        });
    }
    if name.len() > 64 {
        return Err(Error::Decode {
            reason: format!("secret name {} chars; max is 64", name.len()),
        });
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return Err(Error::Decode {
            reason: format!("secret name {name:?} contains invalid chars; allowed [A-Za-z0-9._-]"),
        });
    }
    Ok(())
}
