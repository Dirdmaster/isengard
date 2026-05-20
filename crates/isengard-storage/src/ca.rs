//! Single-row CA storage.
//!
//! The controller owns one CA per installation. Migration `0014` creates
//! the `ca` table with `CHECK (id = 1)`: the only valid row is `id = 1`,
//! the singleton root key + cert.
//!
//! The PEM blobs are NOT additionally encrypted. Filesystem permissions
//! on the SQLite file (chmod 600 in the state-dir) are the only
//! protection. The design spec calls this out as a known limitation:
//! see "CA private key protection" in the controller's enrollment doc.

use sqlx::Row;

use crate::{Inventory, Result};

/// PEM material for the controller's CA.
///
/// `root_cert_pem` is what the agent pins. `root_key_pem` is the
/// signing key for every leaf the controller mints. Both are stored in
/// the same row keyed by `id = 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaRow {
    /// CA root certificate, PEM-encoded.
    pub root_cert_pem: String,
    /// CA root private key, PEM-encoded. Never logged.
    pub root_key_pem: String,
}

impl Inventory {
    /// Fetch the singleton CA row.
    ///
    /// Returns `None` if no CA has been initialised yet (fresh database,
    /// pre-`isd init`).
    pub async fn get_ca(&self) -> Result<Option<CaRow>> {
        let row = sqlx::query("SELECT root_cert_pem, root_key_pem FROM ca WHERE id = 1")
            .fetch_optional(self.pool())
            .await?;

        Ok(row.map(|r| CaRow {
            root_cert_pem: r.get::<String, _>(0),
            root_key_pem: r.get::<String, _>(1),
        }))
    }

    /// Insert the singleton CA row.
    ///
    /// # Errors
    ///
    /// Errors (UNIQUE constraint on `ca.id`) if a CA already exists.
    /// Callers should call [`Inventory::get_ca`] first and only call
    /// `set_ca` when there is no existing row. The single-row guarantee
    /// is what lets the rest of the trust surface treat the CA as a
    /// process-wide constant.
    pub async fn set_ca(&self, row: CaRow) -> Result<()> {
        sqlx::query("INSERT INTO ca (id, root_cert_pem, root_key_pem) VALUES (1, ?, ?)")
            .bind(&row.root_cert_pem)
            .bind(&row.root_key_pem)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
