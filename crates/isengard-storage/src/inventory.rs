//! `Inventory`: the public CRUD surface over the `hosts` table.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::SqlitePool;

use crate::error::{Error, Result};
use crate::host::{EnrollHost, Host, HostId};

/// Wraps a `sqlx::SqlitePool` opened against a single `.db` file.
/// Cheap to clone (the pool is `Arc`-backed inside).
#[derive(Debug, Clone)]
pub struct Inventory {
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

    /// Insert a new host. Returns the freshly assigned `HostId`. The
    /// `enrolled_at` timestamp is set to "now" (Unix seconds).
    pub async fn enroll_host(&self, req: EnrollHost) -> Result<HostId> {
        let id = HostId::new();
        let id_bytes: &[u8] = &id.to_bytes();
        let enrolled_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        sqlx::query(
            r#"
            INSERT INTO hosts (
                id, fingerprint, hostname, os, arch,
                agent_version, docker_version, enrolled_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id_bytes)
        .bind(&req.fingerprint)
        .bind(&req.hostname)
        .bind(&req.os)
        .bind(&req.arch)
        .bind(&req.agent_version)
        .bind(&req.docker_version)
        .bind(enrolled_at)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Look up a host by id. Returns `None` if no row matches.
    pub async fn get_host(&self, id: HostId) -> Result<Option<Host>> {
        let id_bytes: &[u8] = &id.to_bytes();

        let row: Option<HostRow> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata
            FROM hosts
            WHERE id = ?
            "#,
        )
        .bind(id_bytes)
        .fetch_optional(&self.pool)
        .await?;

        row.map(decode_host).transpose()
    }

    /// Borrow the underlying pool. Used by inventory methods (and tests that
    /// want to peek at table state).
    #[allow(dead_code)] // consumed by inventory CRUD methods in later tasks
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

type HostRow = (
    Vec<u8>,     // id
    String,      // fingerprint
    String,      // hostname
    String,      // os
    String,      // arch
    String,      // agent_version
    String,      // docker_version
    i64,         // enrolled_at
    Option<i64>, // last_seen_at
    String,      // metadata (json text)
);

fn decode_host(row: HostRow) -> Result<Host> {
    let id_bytes: [u8; 16] = row
        .0
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidHostId(row.0.len()))?;
    let metadata: serde_json::Value = serde_json::from_str(&row.9).map_err(|e| Error::Decode {
        reason: format!("metadata json: {e}"),
    })?;

    Ok(Host {
        id: HostId::from_bytes(id_bytes),
        fingerprint: row.1,
        hostname: row.2,
        os: row.3,
        arch: row.4,
        agent_version: row.5,
        docker_version: row.6,
        enrolled_at: row.7,
        last_seen_at: row.8,
        metadata,
    })
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

    fn sample_enrollment() -> EnrollHost {
        EnrollHost {
            fingerprint: "ada-lovelace.example".into(),
            hostname: "ada-lovelace".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0-alpha".into(),
            docker_version: "27.4.0".into(),
        }
    }

    #[tokio::test]
    async fn enroll_then_get_round_trips() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let id = inv.enroll_host(req.clone()).await.unwrap();

        let got = inv.get_host(id).await.unwrap().expect("host should exist");

        assert_eq!(got.id, id);
        assert_eq!(got.fingerprint, req.fingerprint);
        assert_eq!(got.hostname, req.hostname);
        assert_eq!(got.os, req.os);
        assert_eq!(got.arch, req.arch);
        assert_eq!(got.agent_version, req.agent_version);
        assert_eq!(got.docker_version, req.docker_version);
        assert!(got.enrolled_at > 0);
        assert_eq!(got.last_seen_at, None);
        assert_eq!(got.metadata, serde_json::json!({}));
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_id() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let result = inv.get_host(HostId::new()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn duplicate_fingerprint_is_rejected() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let req = sample_enrollment();
        let _ = inv.enroll_host(req.clone()).await.unwrap();
        let err = inv
            .enroll_host(req)
            .await
            .expect_err("dup fingerprint must error");
        assert!(matches!(err, Error::Db(_)), "unexpected error: {err:?}");
    }
}
