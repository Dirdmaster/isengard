//! Settings: per-installation JSON key/value store.
//!
//! Used by the notifier plugins and other features that need durable
//! configuration without earning a dedicated table. Read via
//! [`crate::Inventory::get_setting`], write via
//! [`crate::Inventory::set_setting`].
//!
//! [`SettingStore`] is a thin wrapper around the same table for callers
//! that want CRUD without going through [`crate::Inventory`]. The
//! `isd configure` dispatcher uses it as its non-secret backing store.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Inventory;
use crate::error::Result;

/// One row from the `settings` table.
///
/// The shape is intentionally narrow: a string key, an arbitrary JSON
/// value, and the timestamp of the last write. Callers serialize their
/// typed config into `value` and decide what schema lives there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// Setting key, e.g. `notifier.telegram.enabled`.
    pub key: String,
    /// JSON value bound to `key`. The DAO does not interpret the shape.
    pub value: serde_json::Value,
    /// When the row was last written (RFC3339).
    pub updated_at: DateTime<Utc>,
}

/// CRUD wrapper around the `settings` table.
///
/// Mirrors the shape of the existing [`Inventory`] helpers
/// (`set_setting`, `get_setting`, `list_settings`) but exposes them as
/// dedicated methods on a small handle. The `isd configure`
/// dispatcher takes one of these alongside a secrets store and routes
/// per-key based on the static schema.
///
/// `created_by` on [`SettingStore::upsert`] is accepted for parity with
/// the secrets store but is not persisted today; the `settings` table
/// has no audit column. Pass it anyway so callers do not need a
/// secret-vs-setting branch.
#[derive(Clone)]
pub struct SettingStore {
    /// Shared inventory handle. Cloning is cheap (sqlx pool is `Arc`
    /// inside).
    inv: Arc<Inventory>,
}

impl SettingStore {
    /// Wraps an existing [`Inventory`] handle.
    pub fn new(inv: Arc<Inventory>) -> Self {
        Self { inv }
    }

    /// Insert or replace one key.
    ///
    /// `_created_by` is accepted for parity with the secrets DAO but
    /// not stored: the `settings` table has no such column.
    pub async fn upsert(
        &self,
        key: &str,
        value: &serde_json::Value,
        _created_by: Option<&str>,
    ) -> Result<()> {
        self.inv.set_setting(key, value).await
    }

    /// Fetch one key, or `None` when no row matches.
    pub async fn get(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.inv.get_setting(key).await
    }

    /// Delete one key. Returns `true` when a row was removed.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(self.inv.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Every row, sorted by key.
    pub async fn list(&self) -> Result<Vec<Setting>> {
        self.inv.list_settings().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Inventory;
    use serde_json::json;

    async fn store() -> SettingStore {
        let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
        SettingStore::new(inv)
    }

    #[tokio::test]
    async fn upsert_inserts_new_row() {
        let s = store().await;
        s.upsert("acme.directory", &json!("https://example.com"), Some("test"))
            .await
            .unwrap();
        let v = s.get("acme.directory").await.unwrap();
        assert_eq!(v, Some(json!("https://example.com")));
    }

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let s = store().await;
        s.upsert("k", &json!("a"), None).await.unwrap();
        s.upsert("k", &json!("b"), None).await.unwrap();
        assert_eq!(s.get("k").await.unwrap(), Some(json!("b")));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let s = store().await;
        s.upsert("k", &json!(1), None).await.unwrap();
        assert!(s.delete("k").await.unwrap());
        assert_eq!(s.get("k").await.unwrap(), None);
        assert!(!s.delete("k").await.unwrap());
    }

    #[tokio::test]
    async fn list_returns_all_rows_sorted() {
        let s = store().await;
        s.upsert("b.x", &json!(1), None).await.unwrap();
        s.upsert("a.y", &json!(2), None).await.unwrap();
        let rows = s.list().await.unwrap();
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["a.y", "b.x"]);
    }
}
