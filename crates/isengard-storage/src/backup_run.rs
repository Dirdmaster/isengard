//! `backup_runs` DAO.
//!
//! Migration `0019` lands `backup_runs`. A row is inserted with
//! status `running` when a snapshot starts, then transitioned to
//! `success` (with `object_name` + `size_bytes`) or `failed` (with an
//! error string) when finished. The dashboard's runs listing reads
//! this table, ordered newest-first.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::inventory::Inventory;

/// Status of a backup run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupRunStatus {
    /// Snapshot in progress.
    Running,
    /// Snapshot completed cleanly; object exists at the recorded name.
    Success,
    /// Snapshot aborted; error string holds the reason.
    Failed,
}

impl BackupRunStatus {
    /// Canonical lowercase spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            BackupRunStatus::Running => "running",
            BackupRunStatus::Success => "success",
            BackupRunStatus::Failed => "failed",
        }
    }

    /// Parse a status TEXT column. Returns `None` for unknown strings.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(BackupRunStatus::Running),
            "success" => Some(BackupRunStatus::Success),
            "failed" => Some(BackupRunStatus::Failed),
            _ => None,
        }
    }
}

/// Stable id type for backup runs (autoincrement integer in SQLite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRunId(pub i64);

/// A row from `backup_runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRun {
    /// Surrogate key.
    pub id: BackupRunId,
    /// When the snapshot started.
    pub started_at: DateTime<Utc>,
    /// When the snapshot finished (success or failure).
    pub finished_at: Option<DateTime<Utc>>,
    /// Status of the run.
    pub status: BackupRunStatus,
    /// Object name in the remote store. Set on success only.
    pub object_name: Option<String>,
    /// Size of the uploaded object in bytes.
    pub size_bytes: Option<i64>,
    /// Failure reason string. Set on failure only.
    pub error: Option<String>,
}

impl Inventory {
    /// Insert a new run with status `running`. Returns the assigned id.
    pub async fn insert_backup_run(&self, started_at: DateTime<Utc>) -> Result<BackupRunId> {
        let r = sqlx::query("INSERT INTO backup_runs (started_at, status) VALUES (?, 'running')")
            .bind(started_at.to_rfc3339())
            .execute(self.pool())
            .await?;
        Ok(BackupRunId(r.last_insert_rowid()))
    }

    /// Mark a run as `success`, setting `finished_at`, `object_name`,
    /// `size_bytes`.
    pub async fn finish_backup_run_success(
        &self,
        id: BackupRunId,
        finished_at: DateTime<Utc>,
        object_name: &str,
        size_bytes: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE backup_runs SET status = 'success', finished_at = ?, \
             object_name = ?, size_bytes = ? WHERE id = ?",
        )
        .bind(finished_at.to_rfc3339())
        .bind(object_name)
        .bind(size_bytes)
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Mark a run as `failed`, setting `finished_at` and `error`.
    pub async fn finish_backup_run_failed(
        &self,
        id: BackupRunId,
        finished_at: DateTime<Utc>,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE backup_runs SET status = 'failed', finished_at = ?, error = ? WHERE id = ?",
        )
        .bind(finished_at.to_rfc3339())
        .bind(error)
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// List the most recent runs, newest-first. `limit` clamps to `1..200`.
    pub async fn list_backup_runs(&self, limit: u32) -> Result<Vec<BackupRun>> {
        let limit = limit.clamp(1, 200);
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT id, started_at, finished_at, status, object_name, size_bytes, error \
             FROM backup_runs ORDER BY id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|r| {
                let started_s: String = r.try_get("started_at")?;
                let finished_s: Option<String> = r.try_get("finished_at")?;
                let status_s: String = r.try_get("status")?;
                let status = BackupRunStatus::parse(&status_s).ok_or_else(|| {
                    crate::error::Error::Decode {
                        reason: format!("unknown backup run status: {status_s}"),
                    }
                })?;

                Ok(BackupRun {
                    id: BackupRunId(r.try_get("id")?),
                    started_at: parse_rfc3339(&started_s)?,
                    finished_at: match finished_s {
                        Some(s) => Some(parse_rfc3339(&s)?),
                        None => None,
                    },
                    status,
                    object_name: r.try_get("object_name")?,
                    size_bytes: r.try_get("size_bytes")?,
                    error: r.try_get("error")?,
                })
            })
            .collect()
    }

    /// The most recent successful run, or `None` when no run has
    /// succeeded yet. Walks at most 50 rows.
    pub async fn last_successful_backup_run(&self) -> Result<Option<BackupRun>> {
        let runs = self.list_backup_runs(50).await?;
        Ok(runs
            .into_iter()
            .find(|r| r.status == BackupRunStatus::Success))
    }
}

/// Parse an RFC3339 string into `DateTime<Utc>`.
fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| crate::error::Error::Decode {
            reason: format!("rfc3339 parse: {e}"),
        })
}
