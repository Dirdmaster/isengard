//! `Inventory`: the public CRUD surface over the `hosts` table.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::SqlitePool;

use crate::error::Result;

/// Wraps a `sqlx::SqlitePool` opened against a single `.db` file.
/// Cheap to clone (the pool is `Arc`-backed inside).
#[derive(Debug, Clone)]
pub struct Inventory {
    #[allow(dead_code)] // consumed by inventory CRUD methods in later tasks
    pool: SqlitePool,
}

impl Inventory {
    /// Open (or create) the database at `path` and run all pending migrations.
    /// The parent directory must exist; the file is created if missing.
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(opts).await?;
        sqlx::migrate!().run(&pool).await?;

        Ok(Self { pool })
    }

    /// Open an in-memory database. Useful for tests; the data is wiped when
    /// the `Inventory` is dropped.
    pub async fn open_in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    /// Borrow the underlying pool. Used by inventory methods (and tests that
    /// want to peek at table state).
    #[allow(dead_code)] // consumed by inventory CRUD methods in later tasks
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn open_creates_file_and_runs_migrations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let inv = Inventory::open(&path).await.expect("open");
        assert!(path.exists(), "db file should be created");

        // Migration should have created the hosts table — check by querying
        // sqlite_master (sqlite's catalog).
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hosts'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "hosts table should exist after migrate");
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let inv = Inventory::open_in_memory().await.expect("open");
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='hosts_last_seen_at_idx'",
        )
        .fetch_one(inv.pool())
        .await
        .expect("query");
        assert_eq!(row.0, 1, "last_seen_at index should exist");
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("isengard.db");

        let _inv1 = Inventory::open(&path).await.expect("open 1");
        // Reopen the same file — migrations should be a no-op the second time.
        let _inv2 = Inventory::open(&path).await.expect("open 2");
    }
}
