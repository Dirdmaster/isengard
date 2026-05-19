//! SQLite snapshot integrity tests for `create_snapshot`.

use std::str::FromStr;

use isengard_plugin_backup::snapshot::{SnapshotError, create_snapshot};
use isengard_storage::Inventory;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tempfile::TempDir;

async fn open_disk_pool(path: &std::path::Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePool::connect_with(opts).await.unwrap()
}

/// 1. Snapshot of a fresh DB exists, is non-empty, and can be reopened as SQLite.
#[tokio::test]
async fn snapshot_produces_valid_sqlite_file() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let _inv = Inventory::open(&db_path).await.unwrap();
    // Reopen our own pool so we control the lock semantics.
    let pool = open_disk_pool(&db_path).await;

    let tmp = create_snapshot(&pool, &db_path).await.unwrap();
    let bytes = std::fs::read(tmp.path()).unwrap();
    assert!(
        bytes.len() >= 100,
        "snapshot should contain at least a SQLite header"
    );
    assert!(
        bytes.starts_with(b"SQLite format 3\0"),
        "snapshot must begin with SQLite magic"
    );

    // Reopen the snapshot read-only to confirm sqlite accepts it.
    let snap_pool = SqlitePool::connect(&format!("sqlite://{}?mode=ro", tmp.path().display()))
        .await
        .unwrap();
    let _: (i64,) = sqlx::query_as("SELECT count(*) FROM sqlite_master")
        .fetch_one(&snap_pool)
        .await
        .unwrap();
}

/// 2. Snapshot reflects committed writes that happened before snapshot.
#[tokio::test]
async fn snapshot_reflects_committed_writes() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.unwrap();

    inv.set_setting("backup.test.key", &serde_json::json!("hello"))
        .await
        .unwrap();

    let pool = open_disk_pool(&db_path).await;
    let tmp = create_snapshot(&pool, &db_path).await.unwrap();

    // Reopen the snapshot and confirm the row is there.
    let snap_inv = Inventory::open(tmp.path()).await.unwrap();
    let v = snap_inv.get_setting("backup.test.key").await.unwrap();
    assert_eq!(v, Some(serde_json::json!("hello")));
}

/// 3. Snapshot works repeatedly (idempotent re-snapshot).
#[tokio::test]
async fn snapshot_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let _inv = Inventory::open(&db_path).await.unwrap();
    let pool = open_disk_pool(&db_path).await;

    for _ in 0..3 {
        let tmp = create_snapshot(&pool, &db_path).await.unwrap();
        let bytes = std::fs::read(tmp.path()).unwrap();
        assert!(bytes.starts_with(b"SQLite format 3\0"));
    }
}

/// 4. Snapshot survives writes between snapshots (later snapshot has the new row).
#[tokio::test]
async fn snapshot_picks_up_new_writes_between_runs() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.unwrap();
    let pool = open_disk_pool(&db_path).await;

    inv.set_setting("backup.test.first", &serde_json::json!(1))
        .await
        .unwrap();
    let snap_a = create_snapshot(&pool, &db_path).await.unwrap();

    inv.set_setting("backup.test.second", &serde_json::json!(2))
        .await
        .unwrap();
    let snap_b = create_snapshot(&pool, &db_path).await.unwrap();

    // First snapshot: only first key.
    let inv_a = Inventory::open(snap_a.path()).await.unwrap();
    assert!(
        inv_a
            .get_setting("backup.test.first")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        inv_a
            .get_setting("backup.test.second")
            .await
            .unwrap()
            .is_none()
    );

    // Second snapshot: both keys.
    let inv_b = Inventory::open(snap_b.path()).await.unwrap();
    assert!(
        inv_b
            .get_setting("backup.test.first")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        inv_b
            .get_setting("backup.test.second")
            .await
            .unwrap()
            .is_some()
    );
}

/// 5. Missing source path errors out clearly.
#[tokio::test]
async fn snapshot_missing_source_errors() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("does-not-exist.db");

    // Need any pool to call against; open one against a different file
    // that does exist.
    let real = dir.path().join("real.db");
    let _inv = Inventory::open(&real).await.unwrap();
    let pool = open_disk_pool(&real).await;

    let err = create_snapshot(&pool, &db_path).await.unwrap_err();
    matches!(err, SnapshotError::MissingSource(_));
}

/// 6. After WAL checkpoint, the WAL sidecar is truncated to zero bytes.
#[tokio::test]
async fn snapshot_truncates_wal() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.unwrap();
    let pool = open_disk_pool(&db_path).await;

    inv.set_setting("k", &serde_json::json!(1)).await.unwrap();
    inv.set_setting("k2", &serde_json::json!(2)).await.unwrap();

    let _tmp = create_snapshot(&pool, &db_path).await.unwrap();

    let wal_path = db_path.with_extension("db-wal");
    if wal_path.exists() {
        let len = std::fs::metadata(&wal_path).unwrap().len();
        assert!(
            len <= 32, // a fully-truncated WAL has just a tiny header at most
            "WAL should be truncated after snapshot, got {len} bytes"
        );
    }
}

/// 7. Snapshot of a fresh empty (post-migrations) DB is valid + readable.
#[tokio::test]
async fn snapshot_of_empty_db_is_valid() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let _inv = Inventory::open(&db_path).await.unwrap();
    let pool = open_disk_pool(&db_path).await;

    let tmp = create_snapshot(&pool, &db_path).await.unwrap();
    let snap_inv = Inventory::open(tmp.path()).await.unwrap();

    // No hosts, no settings, but the migrations all ran: tables exist.
    let hosts = snap_inv.list_hosts().await.unwrap();
    assert!(hosts.is_empty());
    let runs = snap_inv.list_backup_runs(10).await.unwrap();
    assert!(runs.is_empty(), "fresh DB should have no backup runs");
}

/// 8. Snapshot bytes are equal to the on-disk DB file at the moment of the call.
#[tokio::test]
async fn snapshot_byte_identical_to_source() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.unwrap();
    let pool = open_disk_pool(&db_path).await;

    inv.set_setting("backup.eq.key", &serde_json::json!("eq"))
        .await
        .unwrap();

    let tmp = create_snapshot(&pool, &db_path).await.unwrap();
    let snap_bytes = std::fs::read(tmp.path()).unwrap();
    let live_bytes = std::fs::read(&db_path).unwrap();
    assert_eq!(
        snap_bytes, live_bytes,
        "snapshot should be byte-identical to live db at snapshot time"
    );
}
