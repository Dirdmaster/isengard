#![doc = include_str!("../docs/restore.md")]

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use isengard_storage::{Inventory, RestoreRunId};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use tempfile::NamedTempFile;
use tracing::{info, warn};

use crate::destination::BackupDestination;
use crate::encrypt::decrypt_with_passphrase;

/// Result of a successful restore attempt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RestoreOutcome {
    /// `restore_runs` row id.
    pub run_id: i64,
    /// Object name on the destination that was restored.
    pub source_object: String,
    /// Wall-clock instant the swap completed (or the dry-run
    /// finished validating).
    pub restored_at: DateTime<Utc>,
    /// Path the previous DB was renamed to before the new file
    /// moved into place. Empty for `dry_run` (no swap was
    /// performed).
    pub previous_db_backup_path: String,
    /// Bytes written to the live `db_path`. `0` for `dry_run`.
    pub bytes_restored: u64,
    /// `true` when the caller passed `dry_run = true`; no on-disk
    /// side effects.
    pub dry_run: bool,
}

/// Errors raised during a restore.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// Destination download or list failed.
    #[error("destination: {0}")]
    Destination(#[from] crate::destination::DestinationError),

    /// Decryption failed (wrong passphrase or corrupted blob).
    #[error("decrypt failed: invalid passphrase or corrupted blob ({0})")]
    Decrypt(crate::encrypt::EncryptError),

    /// Decrypted bytes don't parse as a valid SQLite database.
    #[error("decrypted bytes do not parse as a valid SQLite database: {0}")]
    InvalidSnapshot(String),

    /// Atomic swap failed at the rename step.
    #[error("atomic swap failed: {0}")]
    Swap(String),

    /// Post-restore migrations failed against the new DB file.
    #[error("post-restore migrations failed: {0}")]
    Migrate(String),

    /// Storage DAO failed (e.g. inserting the run row).
    #[error("storage: {0}")]
    Storage(#[from] isengard_storage::Error),

    /// Filesystem IO failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Empty passphrase rejected up front.
    #[error("passphrase is empty")]
    EmptyPassphrase,
}

/// Runs a full restore from `dest`.
///
/// See the module-level docs for the pipeline and atomic-swap
/// guarantees. On dry-run, performs download / decrypt / validate
/// then returns without touching the on-disk DB.
///
/// # Errors
///
/// Returns the [`RestoreError`] that caused the restore to fail.
/// `restore_runs` state is written before returning.
pub async fn restore_from_destination(
    inv: &Arc<Inventory>,
    db_path: &Path,
    dest: &dyn BackupDestination,
    object_name: &str,
    source_backup_run_id: Option<i64>,
    passphrase: &str,
    dry_run: bool,
) -> Result<RestoreOutcome, RestoreError> {
    if passphrase.is_empty() {
        return Err(RestoreError::EmptyPassphrase);
    }

    let started = Utc::now();
    let run_id = inv
        .insert_restore_run(object_name, source_backup_run_id, started)
        .await?;

    let outcome = run_inner(db_path, dest, object_name, passphrase, dry_run, run_id).await;

    match outcome {
        Ok(o) => {
            if o.dry_run {
                inv.finish_restore_run_success(
                    run_id,
                    Utc::now(),
                    &o.previous_db_backup_path,
                    o.bytes_restored as i64,
                )
                .await?;
            } else {
                let new_inv = Inventory::open(db_path)
                    .await
                    .map_err(|e| RestoreError::Migrate(e.to_string()))?;
                let id = new_inv
                    .insert_restore_run(object_name, source_backup_run_id, started)
                    .await?;
                new_inv
                    .finish_restore_run_success(
                        id,
                        Utc::now(),
                        &o.previous_db_backup_path,
                        o.bytes_restored as i64,
                    )
                    .await?;
            }
            info!(
                run_id = run_id.0,
                object = %object_name,
                dry_run,
                "restore run succeeded"
            );
            Ok(o)
        }
        Err(e) => {
            let msg = e.to_string();
            if let Err(e2) = inv
                .finish_restore_run_failed(run_id, Utc::now(), &msg)
                .await
            {
                warn!(error = %e2, run_id = run_id.0, "failed to mark restore as failed");
            }
            warn!(run_id = run_id.0, error = %msg, "restore run failed");
            Err(e)
        }
    }
}

/// Inner pipeline: download, decrypt, validate, swap.
///
/// Split from [`restore_from_destination`] so the outer function
/// owns the `restore_runs` row state machine and this function owns
/// the on-disk side effects.
///
/// # Errors
///
/// Bubbles every stage-specific [`RestoreError`].
async fn run_inner(
    db_path: &Path,
    dest: &dyn BackupDestination,
    object_name: &str,
    passphrase: &str,
    dry_run: bool,
    run_id: RestoreRunId,
) -> Result<RestoreOutcome, RestoreError> {
    let cipher = dest.download(object_name).await?;

    let plain = decrypt_with_passphrase(&cipher, passphrase).map_err(RestoreError::Decrypt)?;

    let staged = NamedTempFile::new()?;
    std::fs::write(staged.path(), &plain)?;
    validate_sqlite(staged.path()).await?;

    if dry_run {
        return Ok(RestoreOutcome {
            run_id: run_id.0,
            source_object: object_name.to_string(),
            restored_at: Utc::now(),
            previous_db_backup_path: String::new(),
            bytes_restored: 0,
            dry_run: true,
        });
    }

    let backup_path = pick_backup_path(db_path, Utc::now());

    let live_wal = wal_sibling(db_path);
    let live_shm = shm_sibling(db_path);

    if !db_path.exists() {
        std::fs::rename(staged.path(), db_path).map_err(|e| {
            RestoreError::Swap(format!("rename staged -> live (no prior file): {e}"))
        })?;
        let _ = std::fs::remove_file(&live_wal);
        let _ = std::fs::remove_file(&live_shm);
    } else {
        std::fs::rename(db_path, &backup_path).map_err(|e| {
            RestoreError::Swap(format!("rename live -> {}: {e}", backup_path.display()))
        })?;

        let _ = std::fs::rename(&live_wal, wal_sibling(&backup_path));
        let _ = std::fs::rename(&live_shm, shm_sibling(&backup_path));

        if let Err(e) = std::fs::rename(staged.path(), db_path) {
            if let Err(rev) = std::fs::rename(&backup_path, db_path) {
                return Err(RestoreError::Swap(format!(
                    "rename staged -> live failed ({e}); revert also failed ({rev}). \
                     Previous DB is at {}",
                    backup_path.display()
                )));
            }
            let _ = std::fs::rename(wal_sibling(&backup_path), &live_wal);
            let _ = std::fs::rename(shm_sibling(&backup_path), &live_shm);
            return Err(RestoreError::Swap(format!(
                "rename staged -> live failed ({e}); reverted to original"
            )));
        }
    }

    let bytes_restored = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    Inventory::open(db_path)
        .await
        .map_err(|e| RestoreError::Migrate(e.to_string()))?;

    Ok(RestoreOutcome {
        run_id: run_id.0,
        source_object: object_name.to_string(),
        restored_at: Utc::now(),
        previous_db_backup_path: backup_path.to_string_lossy().to_string(),
        bytes_restored,
        dry_run: false,
    })
}

/// Opens a temporary SQLite pool against `path` and runs a trivial
/// query to verify the bytes parse as a real database.
///
/// # Errors
///
/// Returns [`RestoreError::InvalidSnapshot`] when the open, the
/// trivial query, or the result check fails.
async fn validate_sqlite(path: &Path) -> Result<(), RestoreError> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .map_err(|e| RestoreError::InvalidSnapshot(e.to_string()))?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(opts)
        .await
        .map_err(|e| RestoreError::InvalidSnapshot(e.to_string()))?;
    let r: (i64,) = sqlx::query_as("SELECT 1")
        .fetch_one(&pool)
        .await
        .map_err(|e| RestoreError::InvalidSnapshot(e.to_string()))?;
    if r.0 != 1 {
        return Err(RestoreError::InvalidSnapshot(
            "SELECT 1 did not return 1".into(),
        ));
    }
    pool.close().await;
    Ok(())
}

/// Picks a unique backup path next to `db_path`.
///
/// Names the path `<db>.bak.<ts>`. When that collides (rare; manual
/// operator action or rapid restores) appends `-N`, with a hard
/// ceiling at 1000 attempts.
fn pick_backup_path(db_path: &Path, when: DateTime<Utc>) -> PathBuf {
    let ts = when.format("%Y%m%dT%H%M%SZ").to_string();
    let base = format!("{}.bak.{}", db_path.display(), ts);
    let candidate = PathBuf::from(&base);
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 1;
    loop {
        let attempt = PathBuf::from(format!("{base}-{n}"));
        if !attempt.exists() {
            return attempt;
        }
        n += 1;
        if n > 1000 {
            return PathBuf::from(format!("{base}-{n}"));
        }
    }
}

/// Returns the WAL sibling path for `db_path`.
fn wal_sibling(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// Returns the SHM sibling path for `db_path`.
fn shm_sibling(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push("-shm");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn pick_backup_path_uses_timestamp_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("isengard.db");
        std::fs::write(&live, b"x").unwrap();
        let when = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap();
        let p = pick_backup_path(&live, when);
        assert_eq!(p, dir.path().join("isengard.db.bak.20260506T120000Z"));
    }

    #[test]
    fn pick_backup_path_appends_suffix_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("isengard.db");
        std::fs::write(&live, b"x").unwrap();
        let when = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap();
        let bak = dir.path().join("isengard.db.bak.20260506T120000Z");
        std::fs::write(&bak, b"old").unwrap();
        let p = pick_backup_path(&live, when);
        assert_eq!(p, dir.path().join("isengard.db.bak.20260506T120000Z-1"));
    }
}
