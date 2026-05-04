//! Single-row CA storage. The PEM blobs are *not* additionally encrypted:
//! file permissions on the SQLite file (chmod 600 in state-dir) are the only
//! protection. See spec §"CA private key protection" for the limitation.

use sqlx::Row;

use crate::{Inventory, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaRow {
    pub root_cert_pem: String,
    pub root_key_pem: String,
}

impl Inventory {
    /// Fetch the singleton CA row (id = 1). Returns `None` if no CA has been
    /// initialised yet.
    pub async fn get_ca(&self) -> Result<Option<CaRow>> {
        let row = sqlx::query("SELECT root_cert_pem, root_key_pem FROM ca WHERE id = 1")
            .fetch_optional(self.pool())
            .await?;

        Ok(row.map(|r| CaRow {
            root_cert_pem: r.get::<String, _>(0),
            root_key_pem: r.get::<String, _>(1),
        }))
    }

    /// Insert the singleton CA row. Errors (UNIQUE constraint on `ca.id`) if
    /// a CA already exists; callers should call [`Inventory::get_ca`] first
    /// and only [`Inventory::set_ca`] when there is no existing row.
    pub async fn set_ca(&self, row: CaRow) -> Result<()> {
        sqlx::query("INSERT INTO ca (id, root_cert_pem, root_key_pem) VALUES (1, ?, ?)")
            .bind(&row.root_cert_pem)
            .bind(&row.root_key_pem)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
