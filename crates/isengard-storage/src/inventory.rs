//! `Inventory`: the public CRUD surface over the `hosts` table.

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

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

    /// Update `last_seen_at` for a host. No-op if the host doesn't exist.
    /// Returns whether a row was actually updated.
    pub async fn touch_host(&self, id: HostId, ts: i64) -> Result<bool> {
        let id_bytes: &[u8] = &id.to_bytes();
        let result = sqlx::query("UPDATE hosts SET last_seen_at = ? WHERE id = ?")
            .bind(ts)
            .bind(id_bytes)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Remove a host from the inventory. Returns true if a row was deleted.
    pub async fn delete_host(&self, id: HostId) -> Result<bool> {
        let id_bytes: &[u8] = &id.to_bytes();
        let result = sqlx::query("DELETE FROM hosts WHERE id = ?")
            .bind(id_bytes)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Return every host, ordered by `last_seen_at DESC` (recently active first;
    /// hosts never seen sort to the bottom).
    pub async fn list_hosts(&self) -> Result<Vec<Host>> {
        let rows: Vec<HostRow> = sqlx::query_as(
            r#"
            SELECT id, fingerprint, hostname, os, arch,
                   agent_version, docker_version, enrolled_at, last_seen_at, metadata
            FROM hosts
            ORDER BY last_seen_at DESC NULLS LAST, enrolled_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(decode_host).collect()
    }

    /// Borrow the underlying pool. Used by inventory methods (and tests that
    /// want to peek at table state).
    #[allow(dead_code)] // only consumed by tests; lib build sees it as unused
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

    #[tokio::test]
    async fn touch_updates_last_seen_for_known_host() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let id = inv.enroll_host(sample_enrollment()).await.unwrap();

        let updated = inv.touch_host(id, 1_700_000_000).await.unwrap();
        assert!(updated);

        let host = inv.get_host(id).await.unwrap().unwrap();
        assert_eq!(host.last_seen_at, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn touch_unknown_host_returns_false() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let updated = inv.touch_host(HostId::new(), 1_700_000_000).await.unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn list_returns_recently_seen_first() {
        let inv = Inventory::open_in_memory().await.unwrap();

        // Enroll two hosts with different fingerprints.
        let mut req_a = sample_enrollment();
        req_a.fingerprint = "host-a.example".into();
        let id_a = inv.enroll_host(req_a).await.unwrap();

        let mut req_b = sample_enrollment();
        req_b.fingerprint = "host-b.example".into();
        let id_b = inv.enroll_host(req_b).await.unwrap();

        // Touch B more recently than A.
        inv.touch_host(id_a, 1_700_000_000).await.unwrap();
        inv.touch_host(id_b, 1_700_000_500).await.unwrap();

        let listed = inv.list_hosts().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, id_b, "more recent host should come first");
        assert_eq!(listed[1].id, id_a);
    }

    #[tokio::test]
    async fn list_empty_inventory_returns_empty_vec() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let listed = inv.list_hosts().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn delete_host_removes_entry() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let enroll = EnrollHost {
            fingerprint: "fp-delete".into(),
            hostname: "h1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "test".into(),
            docker_version: "test".into(),
        };
        let id = inv.enroll_host(enroll).await.unwrap();
        let removed = inv.delete_host(id).await.unwrap();
        assert!(removed);
        assert!(inv.get_host(id).await.unwrap().is_none());
        let removed_again = inv.delete_host(id).await.unwrap();
        assert!(!removed_again);
    }
}
