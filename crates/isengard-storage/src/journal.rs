//! Append-only journal of events emitted by agents (or the controller itself).

use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};

use crate::error::{Error, Result};
use crate::host::HostId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub host_id: Option<HostId>,
    pub kind: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata_json: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InsertEvent {
    pub host_id: Option<HostId>,
    pub kind: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
    pub error: Option<String>,
    pub summary: String,
    pub metadata_json: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// Append-only event journal backed by SQLite.
///
/// The journal can share the same SQLite file as the [`crate::Inventory`] —
/// both call into the same pool of migrations.
#[derive(Debug, Clone)]
pub struct Journal {
    pool: SqlitePool,
}

impl Journal {
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
    /// the `Journal` is dropped.
    pub async fn open_in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    /// Insert a new event row and return its assigned `id`.
    pub async fn insert(&self, ev: InsertEvent) -> Result<i64> {
        let host_id_bytes = ev.host_id.as_ref().map(|h| h.to_bytes().to_vec());
        let occurred = ev.occurred_at.to_rfc3339();
        let row = sqlx::query(
            "INSERT INTO events (host_id, kind, container_name, image, old_digest, new_digest, error, summary, metadata_json, occurred_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(host_id_bytes)
        .bind(&ev.kind)
        .bind(&ev.container_name)
        .bind(&ev.image)
        .bind(&ev.old_digest)
        .bind(&ev.new_digest)
        .bind(&ev.error)
        .bind(&ev.summary)
        .bind(&ev.metadata_json)
        .bind(&occurred)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Most-recent first, capped at `limit`.
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<EventRow>> {
        let rows = sqlx::query(
            "SELECT id, host_id, kind, container_name, image, old_digest, new_digest, error, summary, metadata_json, occurred_at, received_at
             FROM events
             ORDER BY occurred_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_event).collect()
    }
}

fn row_to_event(row: sqlx::sqlite::SqliteRow) -> Result<EventRow> {
    let host_id_bytes: Option<Vec<u8>> = row.get(1);
    let host_id = match host_id_bytes {
        Some(b) if b.len() == 16 => {
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&b);
            Some(HostId::from_bytes(arr))
        }
        Some(b) => return Err(Error::InvalidHostId(b.len())),
        None => None,
    };
    let occurred_str: String = row.get(10);
    let received_str: String = row.get(11);
    Ok(EventRow {
        id: row.get(0),
        host_id,
        kind: row.get(2),
        container_name: row.get(3),
        image: row.get(4),
        old_digest: row.get(5),
        new_digest: row.get(6),
        error: row.get(7),
        summary: row.get(8),
        metadata_json: row.get(9),
        occurred_at: DateTime::parse_from_rfc3339(&occurred_str)
            .map_err(|e| Error::Decode {
                reason: format!("bad occurred_at: {e}"),
            })?
            .with_timezone(&Utc),
        received_at: DateTime::parse_from_rfc3339(&received_str)
            .or_else(|_| {
                // SQLite's CURRENT_TIMESTAMP yields "YYYY-MM-DD HH:MM:SS" (UTC, no TZ marker).
                let with_z = format!("{received_str}Z").replace(' ', "T");
                DateTime::parse_from_rfc3339(&with_z)
            })
            .map_err(|e| Error::Decode {
                reason: format!("bad received_at: {e}"),
            })?
            .with_timezone(&Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(kind: &str) -> InsertEvent {
        InsertEvent {
            host_id: None,
            kind: kind.into(),
            container_name: Some("web".into()),
            image: Some("nginx:1.25".into()),
            old_digest: Some("sha256:aaa".into()),
            new_digest: Some("sha256:bbb".into()),
            error: None,
            summary: format!("{kind} happened"),
            metadata_json: None,
            occurred_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn open_in_memory_runs_migrations() {
        let j = Journal::open_in_memory().await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn insert_then_list_round_trips() {
        let j = Journal::open_in_memory().await.unwrap();
        j.insert(make_event("update.success")).await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "update.success");
        assert_eq!(rows[0].container_name.as_deref(), Some("web"));
        assert_eq!(rows[0].new_digest.as_deref(), Some("sha256:bbb"));
    }

    #[tokio::test]
    async fn list_recent_orders_by_occurred_at_desc() {
        let j = Journal::open_in_memory().await.unwrap();
        let mut e1 = make_event("update.success");
        e1.occurred_at = Utc::now() - chrono::Duration::seconds(10);
        let mut e2 = make_event("update.failed");
        e2.occurred_at = Utc::now();
        j.insert(e1).await.unwrap();
        j.insert(e2).await.unwrap();
        let rows = j.list_recent(10).await.unwrap();
        assert_eq!(rows[0].kind, "update.failed");
        assert_eq!(rows[1].kind, "update.success");
    }

    #[tokio::test]
    async fn list_recent_respects_limit() {
        let j = Journal::open_in_memory().await.unwrap();
        for i in 0..5 {
            j.insert(make_event(&format!("update.kind{i}")))
                .await
                .unwrap();
        }
        let rows = j.list_recent(3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn host_id_round_trips_through_sqlite_blob() {
        let j = Journal::open_in_memory().await.unwrap();
        let host = HostId::new();
        let mut ev = make_event("update.checked");
        ev.host_id = Some(host);
        j.insert(ev).await.unwrap();
        let rows = j.list_recent(1).await.unwrap();
        assert_eq!(rows[0].host_id, Some(host));
    }
}
