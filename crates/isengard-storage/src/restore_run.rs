//! Phase 11b: `restore_runs` DAO.
//!
//! A row is inserted with status=`running` when a restore starts, then
//! transitioned to `success` (with `previous_db_backup_path` + `bytes_restored`)
//! or `failed` (with an error string). The dashboard's restore-runs listing
//! reads this table newest-first.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::inventory::Inventory;

/// Status of a restore run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreRunStatus {
    Running,
    Success,
    Failed,
}

impl RestoreRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RestoreRunStatus::Running => "running",
            RestoreRunStatus::Success => "success",
            RestoreRunStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(RestoreRunStatus::Running),
            "success" => Some(RestoreRunStatus::Success),
            "failed" => Some(RestoreRunStatus::Failed),
            _ => None,
        }
    }
}

/// Stable id type for restore runs (autoincrement integer in SQLite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRunId(pub i64);

/// A row from `restore_runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRun {
    pub id: RestoreRunId,
    pub source_object: String,
    pub source_backup_run_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RestoreRunStatus,
    pub previous_db_backup_path: Option<String>,
    pub bytes_restored: Option<i64>,
    pub error: Option<String>,
}

impl Inventory {
    /// Insert a new restore run with status `running`. Returns the assigned id.
    pub async fn insert_restore_run(
        &self,
        source_object: &str,
        source_backup_run_id: Option<i64>,
        started_at: DateTime<Utc>,
    ) -> Result<RestoreRunId> {
        let r = sqlx::query(
            "INSERT INTO restore_runs (source_object, source_backup_run_id, started_at, status) \
             VALUES (?, ?, ?, 'running')",
        )
        .bind(source_object)
        .bind(source_backup_run_id)
        .bind(started_at.to_rfc3339())
        .execute(self.pool())
        .await?;
        Ok(RestoreRunId(r.last_insert_rowid()))
    }

    /// Mark a restore run as `success`, recording the previous-db backup path
    /// and the byte count of the restored file.
    pub async fn finish_restore_run_success(
        &self,
        id: RestoreRunId,
        finished_at: DateTime<Utc>,
        previous_db_backup_path: &str,
        bytes_restored: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE restore_runs SET status = 'success', finished_at = ?, \
             previous_db_backup_path = ?, bytes_restored = ? WHERE id = ?",
        )
        .bind(finished_at.to_rfc3339())
        .bind(previous_db_backup_path)
        .bind(bytes_restored)
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Mark a restore run as `failed`, setting the error string.
    pub async fn finish_restore_run_failed(
        &self,
        id: RestoreRunId,
        finished_at: DateTime<Utc>,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE restore_runs SET status = 'failed', finished_at = ?, error = ? WHERE id = ?",
        )
        .bind(finished_at.to_rfc3339())
        .bind(error)
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// List the most recent restore runs, newest-first. `limit` is clamped to 1..200.
    pub async fn list_restore_runs(&self, limit: u32) -> Result<Vec<RestoreRun>> {
        let limit = limit.clamp(1, 200);
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT id, source_object, source_backup_run_id, started_at, finished_at, \
                    status, previous_db_backup_path, bytes_restored, error \
             FROM restore_runs ORDER BY id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|r| {
                let started_s: String = r.try_get("started_at")?;
                let finished_s: Option<String> = r.try_get("finished_at")?;
                let status_s: String = r.try_get("status")?;
                let status = RestoreRunStatus::parse(&status_s).ok_or_else(|| {
                    crate::error::Error::Decode {
                        reason: format!("unknown restore run status: {status_s}"),
                    }
                })?;

                Ok(RestoreRun {
                    id: RestoreRunId(r.try_get("id")?),
                    source_object: r.try_get("source_object")?,
                    source_backup_run_id: r.try_get("source_backup_run_id")?,
                    started_at: parse_rfc3339(&started_s)?,
                    finished_at: match finished_s {
                        Some(s) => Some(parse_rfc3339(&s)?),
                        None => None,
                    },
                    status,
                    previous_db_backup_path: r.try_get("previous_db_backup_path")?,
                    bytes_restored: r.try_get("bytes_restored")?,
                    error: r.try_get("error")?,
                })
            })
            .collect()
    }
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| crate::error::Error::Decode {
            reason: format!("rfc3339 parse: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Inventory;

    #[tokio::test]
    async fn insert_records_running_status() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let now = Utc::now();
        let id = inv
            .insert_restore_run("snapshot-x.db.age", Some(7), now)
            .await
            .unwrap();
        let runs = inv.list_restore_runs(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, id);
        assert_eq!(runs[0].status, RestoreRunStatus::Running);
        assert_eq!(runs[0].source_object, "snapshot-x.db.age");
        assert_eq!(runs[0].source_backup_run_id, Some(7));
        assert!(runs[0].previous_db_backup_path.is_none());
    }

    #[tokio::test]
    async fn finish_success_sets_metadata() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let now = Utc::now();
        let id = inv
            .insert_restore_run("snapshot-y.db.age", None, now)
            .await
            .unwrap();
        inv.finish_restore_run_success(
            id,
            now + chrono::Duration::seconds(2),
            "/tmp/isengard.db.bak.20260506T120000Z",
            12345,
        )
        .await
        .unwrap();

        let runs = inv.list_restore_runs(10).await.unwrap();
        assert_eq!(runs[0].status, RestoreRunStatus::Success);
        assert_eq!(
            runs[0].previous_db_backup_path.as_deref(),
            Some("/tmp/isengard.db.bak.20260506T120000Z")
        );
        assert_eq!(runs[0].bytes_restored, Some(12345));
    }

    #[tokio::test]
    async fn finish_failed_sets_error() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let now = Utc::now();
        let id = inv
            .insert_restore_run("missing.db.age", None, now)
            .await
            .unwrap();
        inv.finish_restore_run_failed(id, now, "object not found")
            .await
            .unwrap();
        let runs = inv.list_restore_runs(10).await.unwrap();
        assert_eq!(runs[0].status, RestoreRunStatus::Failed);
        assert_eq!(runs[0].error.as_deref(), Some("object not found"));
    }

    #[tokio::test]
    async fn list_orders_newest_first() {
        let inv = Inventory::open_in_memory().await.unwrap();
        let now = Utc::now();
        let _a = inv
            .insert_restore_run("a.db.age", None, now)
            .await
            .unwrap();
        let b = inv
            .insert_restore_run("b.db.age", None, now + chrono::Duration::seconds(1))
            .await
            .unwrap();
        let runs = inv.list_restore_runs(10).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, b);
        assert_eq!(runs[0].source_object, "b.db.age");
    }
}
