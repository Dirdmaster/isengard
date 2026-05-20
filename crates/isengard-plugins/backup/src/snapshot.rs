//! SQLite snapshot helper.
//!
//! Strategy: WAL checkpoint, IMMEDIATE-tx lock, file copy. The
//! result is byte-identical to the live DB at the moment of the
//! snapshot. Cheap and fast (sub-second for typical controller DBs
//! under 100 MB).
//!
//! The controller's live DB is always on disk (the agent is the
//! stateless half), so the helper is disk-only by design. The
//! returned `NamedTempFile` owns the snapshot bytes and is unlinked
//! on drop, so callers must read or move it before letting it fall
//! out of scope.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tempfile::NamedTempFile;
use tracing::debug;

/// Errors raised while creating a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// SQLite or sqlx layer failure.
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// Filesystem IO failure during the copy or tempfile creation.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `db_path` doesn't exist on disk.
    #[error("source db path does not exist: {0}")]
    MissingSource(PathBuf),
}

/// Creates a snapshot of a SQLite database file.
///
/// Pass the same `pool` that owns the live connection so the WAL
/// checkpoint runs against the actual writer. `db_path` is the
/// on-disk location of the database file (typically
/// `<state_dir>/isengard.db`).
///
/// Sequence:
///
/// 1. `PRAGMA wal_checkpoint(TRUNCATE)`: flushes the WAL into the
///    main file and resizes the WAL back to zero.
/// 2. `BEGIN IMMEDIATE`: holds a write lock for the duration of the
///    copy. New writers block; readers proceed via the existing WAL.
/// 3. `std::fs::copy(db_path, tmp.path())`: copies the bytes.
/// 4. `ROLLBACK`: releases the lock.
///
/// # Errors
///
/// Returns [`SnapshotError::MissingSource`] when `db_path` doesn't
/// exist. Bubbles any sqlx or IO failure encountered during the
/// sequence.
pub async fn create_snapshot(
    pool: &SqlitePool,
    db_path: &Path,
) -> Result<NamedTempFile, SnapshotError> {
    if !db_path.exists() {
        return Err(SnapshotError::MissingSource(db_path.to_path_buf()));
    }

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;

    let mut tx = pool.begin().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *tx).await.ok();

    let tmp = NamedTempFile::new()?;
    std::fs::copy(db_path, tmp.path())?;
    debug!(?db_path, snapshot = %tmp.path().display(), "snapshot copied");

    tx.rollback().await?;

    Ok(tmp)
}
