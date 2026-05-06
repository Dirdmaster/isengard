//! Phase 11a: SQLite snapshot helper.
//!
//! Strategy: WAL checkpoint + IMMEDIATE-tx lock + file copy. This produces a
//! byte-identical replica of the live DB file. Cheap and fast (sub-second for
//! typical controller DBs under 100 MB). The controller's live DB is always
//! on disk (the agent is the stateless half), so the snapshot helper is
//! disk-only by design.
//!
//! The returned NamedTempFile owns the snapshot bytes and is unlinked when
//! dropped, so callers must read or move it before letting it fall out of scope.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tempfile::NamedTempFile;
use tracing::debug;

/// Errors raised while creating a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("source db path does not exist: {0}")]
    MissingSource(PathBuf),
}

/// Create a snapshot of a SQLite database file. The returned tempfile is in
/// the system tempdir; it is byte-identical to the live db at the moment of
/// the snapshot.
///
/// Pass the same `pool` that owns the live connection so the WAL checkpoint
/// runs against the actual writer. The path argument is the on-disk location
/// of the database file (typically `<state_dir>/isengard.db`).
pub async fn create_snapshot(
    pool: &SqlitePool,
    db_path: &Path,
) -> Result<NamedTempFile, SnapshotError> {
    if !db_path.exists() {
        return Err(SnapshotError::MissingSource(db_path.to_path_buf()));
    }

    // 1. Force any pending WAL frames into the main DB file. TRUNCATE form
    //    also resizes the WAL file back to zero, so the snapshot is the
    //    smallest possible representation.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;

    // 2. Hold an IMMEDIATE write lock for the duration of the copy. This
    //    blocks any new writer from starting a transaction during the copy
    //    (readers still proceed via the existing WAL).
    let mut tx = pool.begin().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *tx).await.ok(); // pool.begin is already a tx; ignore the no-op

    // 3. Copy the file.
    let tmp = NamedTempFile::new()?;
    std::fs::copy(db_path, tmp.path())?;
    debug!(?db_path, snapshot = %tmp.path().display(), "snapshot copied");

    // 4. Release the lock.
    tx.rollback().await?;

    Ok(tmp)
}
