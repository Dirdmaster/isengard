//! Phase 11b: restore-from-destination flow.
//!
//! Pipeline: download encrypted blob -> decrypt with passphrase -> validate
//! the bytes are a real SQLite database -> rename current DB to a `.bak.<ts>`
//! sibling -> move the restored bytes into the original path -> open a fresh
//! `Inventory` against the new file (which re-runs migrations forward over
//! the snapshot's schema). Each step is a recorded transition on the
//! `restore_runs` row created at entry; failures land the row in `failed`
//! state and (best-effort) revert the swap so the controller is never left
//! pointing at a half-replaced file.
//!
//! Two ordered renames give us atomicity:
//!
//! ```text
//! mv  isengard.db       isengard.db.bak.<utc>
//! mv  restored-tmp.db   isengard.db
//! ```
//!
//! Either both succeed or we revert by `mv isengard.db.bak.<utc> isengard.db`.
//! We NEVER delete the previous DB silently; the `.bak.<ts>` stays on disk so
//! an operator can manually undo even after a successful restore.

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
    pub run_id: i64,
    pub source_object: String,
    pub restored_at: DateTime<Utc>,
    /// Path the previous DB was renamed to before the new file moved into
    /// place. Empty for `dry_run` (no swap was performed).
    pub previous_db_backup_path: String,
    /// Bytes written to the live `db_path`. 0 for `dry_run`.
    pub bytes_restored: u64,
    /// True when the caller passed `dry_run = true`; no on-disk side effects.
    pub dry_run: bool,
}

/// Errors raised during a restore.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("destination: {0}")]
    Destination(#[from] crate::destination::DestinationError),

    #[error("decrypt failed: invalid passphrase or corrupted blob ({0})")]
    Decrypt(crate::encrypt::EncryptError),

    #[error("decrypted bytes do not parse as a valid SQLite database: {0}")]
    InvalidSnapshot(String),

    #[error("atomic swap failed: {0}")]
    Swap(String),

    #[error("post-restore migrations failed: {0}")]
    Migrate(String),

    #[error("storage: {0}")]
    Storage(#[from] isengard_storage::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("passphrase is empty")]
    EmptyPassphrase,
}

/// Run a full restore. See module docs for the pipeline.
///
/// On dry-run, the function performs the download / decrypt / validate steps
/// but skips the on-disk swap, `.bak.<ts>` rename, and migrations. The
/// `restore_runs` row still records the dry-run outcome so the UI can report
/// verification success.
///
/// Recording a successful restore is subtle: a successful swap replaces the
/// live DB file with the snapshot bytes, which means the `running` row we
/// inserted on entry now lives in the renamed `.bak.<ts>` file. After the
/// swap we open a fresh `Inventory` against the new file (this also runs
/// `sqlx::migrate!` forward) and insert a final `success` row there. The
/// `.bak.<ts>` file keeps its `running` row as a forensic trail.
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
            // For dry-runs, we wrote nothing to disk: finalise the row in-place.
            // For real restores, the live DB is now the snapshot bytes, so the
            // `running` row is gone. Open a fresh Inventory against the new
            // file and insert a `success` row there.
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
            // Best-effort: write the failure into whichever Inventory still
            // points at the live DB. If the failure was pre-swap (most
            // common: download / decrypt / validate / dry-run), the original
            // DB and its `running` row are still in place, and this update
            // simply transitions the row to `failed`. If the failure was
            // post-swap and the rollback also failed (extremely rare), the
            // update may apply to the snapshot's stale state; we accept that
            // as a forensic edge case.
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

async fn run_inner(
    db_path: &Path,
    dest: &dyn BackupDestination,
    object_name: &str,
    passphrase: &str,
    dry_run: bool,
    run_id: RestoreRunId,
) -> Result<RestoreOutcome, RestoreError> {
    // 1. Download the encrypted blob.
    let cipher = dest.download(object_name).await?;

    // 2. Decrypt.
    let plain = decrypt_with_passphrase(&cipher, passphrase).map_err(RestoreError::Decrypt)?;

    // 3. Validate: write the bytes into a temp file and try to open them as a
    //    SQLite DB. Reject anything that fails.
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

    // 4. Atomic swap. Pick a unique `.bak.<ts>[-N]` path next to the original.
    let backup_path = pick_backup_path(db_path, Utc::now());

    // The WAL + SHM siblings of the live path belong to the file we are
    // about to displace. After the swap they would be applied on top of the
    // snapshot bytes by SQLite's recovery logic, undoing the restore. We
    // move them aside next to the .bak.<ts> so the operator still has the
    // forensic trail, then delete the live-side siblings outright.
    let live_wal = wal_sibling(db_path);
    let live_shm = shm_sibling(db_path);

    if !db_path.exists() {
        // Nothing to back up; just move the staged file into place.
        std::fs::rename(staged.path(), db_path).map_err(|e| {
            RestoreError::Swap(format!("rename staged -> live (no prior file): {e}"))
        })?;
        let _ = std::fs::remove_file(&live_wal);
        let _ = std::fs::remove_file(&live_shm);
    } else {
        // Step 4a: rename live -> .bak.<ts>.
        std::fs::rename(db_path, &backup_path).map_err(|e| {
            RestoreError::Swap(format!(
                "rename live -> {}: {e}",
                backup_path.display()
            ))
        })?;

        // Move WAL/SHM siblings to the backup path so they are not picked
        // up by SQLite when it next opens the live path.
        let _ = std::fs::rename(&live_wal, wal_sibling(&backup_path));
        let _ = std::fs::rename(&live_shm, shm_sibling(&backup_path));

        // Step 4b: rename staged -> live. If this fails, revert.
        if let Err(e) = std::fs::rename(staged.path(), db_path) {
            // Revert: rename the backup back to its original name.
            if let Err(rev) = std::fs::rename(&backup_path, db_path) {
                return Err(RestoreError::Swap(format!(
                    "rename staged -> live failed ({e}); revert also failed ({rev}). \
                     Previous DB is at {}",
                    backup_path.display()
                )));
            }
            // Best-effort: move the WAL/SHM back too.
            let _ = std::fs::rename(wal_sibling(&backup_path), &live_wal);
            let _ = std::fs::rename(shm_sibling(&backup_path), &live_shm);
            return Err(RestoreError::Swap(format!(
                "rename staged -> live failed ({e}); reverted to original"
            )));
        }
    }

    let bytes_restored = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    // 5. Migrate forward. Opening a fresh Inventory runs sqlx::migrate! which
    //    is idempotent and forward-only.
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

/// Open a temporary SQLite pool against `path` and run a trivial query to
/// verify the bytes parse as a real database.
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

/// Pick a unique backup path next to `db_path`. If `<db>.bak.<ts>` already
/// exists (rare; manual operator action or rapid restores), append `-N`.
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
            // Hard ceiling; vanishingly unlikely to be reached.
            return PathBuf::from(format!("{base}-{n}"));
        }
    }
}

fn wal_sibling(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

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
        assert_eq!(
            p,
            dir.path().join("isengard.db.bak.20260506T120000Z")
        );
    }

    #[test]
    fn pick_backup_path_appends_suffix_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("isengard.db");
        std::fs::write(&live, b"x").unwrap();
        let when = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap();
        // Pre-create a colliding bak file.
        let bak = dir.path().join("isengard.db.bak.20260506T120000Z");
        std::fs::write(&bak, b"old").unwrap();
        let p = pick_backup_path(&live, when);
        assert_eq!(
            p,
            dir.path().join("isengard.db.bak.20260506T120000Z-1")
        );
    }
}
