//! ACME account singleton. Stores the account key + directory URL + kid for
//! the controller's ACME client. There is at most one row (`CHECK (id = 1)`).
//! See spec §7 — `acme_account` table.

use crate::error::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeAccount {
    pub contact_email: String,
    pub directory_url: String,
    pub account_key_pem: String,
    pub kid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertAcmeAccount {
    pub contact_email: String,
    pub directory_url: String,
    pub account_key_pem: String,
    pub kid: Option<String>,
}

impl crate::inventory::Inventory {
    pub async fn upsert_acme_account(&self, ins: UpsertAcmeAccount) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO acme_account (id, contact_email, directory_url, account_key_pem, kid)
            VALUES (1, ?, ?, ?, ?)
            ON CONFLICT (id) DO UPDATE SET
              contact_email   = excluded.contact_email,
              directory_url   = excluded.directory_url,
              account_key_pem = excluded.account_key_pem,
              kid             = excluded.kid
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

    pub async fn get_acme_account(&self) -> Result<Option<AcmeAccount>> {
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT contact_email, directory_url, account_key_pem, kid \
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

        Ok(Some(AcmeAccount {
            contact_email,
            directory_url,
            account_key_pem,
            kid,
        }))
    }
}
