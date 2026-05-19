//! End-to-end backup pipeline integration test.
//!
//! Simulates a configured controller: writes a row, runs the runner, then
//! verifies the encrypted blob round-trips: download from the destination,
//! decrypt with the configured passphrase, and confirm the bytes match a
//! fresh snapshot of the source DB.

use std::str::FromStr;
use std::sync::Arc;

use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;

/// Tests in this file all touch the process-global `ISENGARD_BACKUP_PASSPHRASE`
/// env var. cargo test runs them in parallel by default, so we serialize via
/// a process-wide async mutex (safe to hold across `.await`).
async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    let m = LOCK.get_or_init(|| AsyncMutex::new(()));
    m.lock().await
}

use isengard_plugin_backup::config::{BackupConfig, DestinationConfig};
use isengard_plugin_backup::destination::{BackupDestination, LocalDestination};
use isengard_plugin_backup::encrypt::{decrypt_with_passphrase, passphrase_fingerprint};
use isengard_plugin_backup::runner::{BackupRunner, PASSPHRASE_ENV};
use isengard_storage::{BackupRunStatus, Inventory};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tempfile::TempDir;

async fn open_disk_pool(path: &std::path::Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePool::connect_with(opts).await.unwrap()
}

#[tokio::test]
async fn end_to_end_local_destination_round_trip() {
    let _guard = env_lock().await;
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let dest_root = dir.path().join("backups");

    // Set up the controller-side state.
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());
    inv.set_setting("seed.key", &serde_json::json!("hello"))
        .await
        .unwrap();

    // Configure the backup plugin via settings.
    let pass = "integration-test-passphrase";
    let cfg = BackupConfig {
        enabled: true,
        destination: DestinationConfig::Local {
            root: dest_root.to_string_lossy().to_string(),
            prefix: "controllers/test".to_string(),
        },
        interval_secs: 86_400,
        retention_keep: 14,
        passphrase_fingerprint: passphrase_fingerprint(pass),
    };
    cfg.save(&inv).await.unwrap();

    // Set the env var the runner needs.
    // SAFETY: tests are single-threaded by default with cargo test --test, but
    // env vars are process-global. We pick a unique value so any concurrent
    // runner doesn't collide.
    unsafe { std::env::set_var(PASSPHRASE_ENV, pass) };

    let pool = open_disk_pool(&db_path).await;
    let runner = BackupRunner::new(inv.clone(), pool, db_path.clone());

    let run_id = runner.run_once().await.unwrap();

    // Check the run was recorded as success.
    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
    assert_eq!(runs[0].status, BackupRunStatus::Success);
    let object_name = runs[0].object_name.as_ref().unwrap().clone();
    assert!(object_name.starts_with("snapshot-"));
    assert!(object_name.ends_with(".db.age"));
    assert!(runs[0].size_bytes.unwrap() > 0);

    // Download the object from the local destination and decrypt it.
    let dest = LocalDestination::new(&dest_root, "controllers/test");
    let cipher = dest.download(&object_name).await.unwrap();
    let plain = decrypt_with_passphrase(&cipher, pass).unwrap();

    // The decrypted bytes are a SQLite database snapshot; verify they parse
    // and the seeded row is present.
    let snap_path = dir.path().join("decrypted.db");
    std::fs::write(&snap_path, &plain).unwrap();
    let snap_inv = Inventory::open(&snap_path).await.unwrap();
    let v = snap_inv.get_setting("seed.key").await.unwrap();
    assert_eq!(v, Some(serde_json::json!("hello")));

    unsafe { std::env::remove_var(PASSPHRASE_ENV) };
}

#[tokio::test]
async fn run_fails_when_passphrase_missing() {
    let _guard = env_lock().await;
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let dest_root = dir.path().join("backups");
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let cfg = BackupConfig {
        enabled: true,
        destination: DestinationConfig::Local {
            root: dest_root.to_string_lossy().to_string(),
            prefix: "x".to_string(),
        },
        ..BackupConfig::default()
    };
    cfg.save(&inv).await.unwrap();

    // Make sure the env var is unset for this test.
    unsafe { std::env::remove_var(PASSPHRASE_ENV) };

    let pool = open_disk_pool(&db_path).await;
    let runner = BackupRunner::new(inv.clone(), pool, db_path.clone());

    let err = runner.run_once().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("passphrase not set"),
        "expected passphrase-missing error, got {msg}"
    );

    // The failed run should still be recorded.
    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, BackupRunStatus::Failed);
}

#[tokio::test]
async fn retention_prunes_oldest_when_count_exceeds_keep() {
    let _guard = env_lock().await;
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let dest_root = dir.path().join("backups");
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let pass = "retention-pass-123";
    let cfg = BackupConfig {
        enabled: true,
        destination: DestinationConfig::Local {
            root: dest_root.to_string_lossy().to_string(),
            prefix: "ret".to_string(),
        },
        interval_secs: 60,
        retention_keep: 2,
        passphrase_fingerprint: passphrase_fingerprint(pass),
    };
    cfg.save(&inv).await.unwrap();

    unsafe { std::env::set_var(PASSPHRASE_ENV, pass) };

    let pool = open_disk_pool(&db_path).await;
    let runner = BackupRunner::new(inv.clone(), pool, db_path.clone());

    // Pre-populate the destination with 3 fake older snapshots.
    let dest = LocalDestination::new(&dest_root, "ret");
    dest.upload("snapshot-20260101T000000Z.db.age", b"old1")
        .await
        .unwrap();
    dest.upload("snapshot-20260102T000000Z.db.age", b"old2")
        .await
        .unwrap();
    dest.upload("snapshot-20260103T000000Z.db.age", b"old3")
        .await
        .unwrap();

    runner.run_once().await.unwrap();

    let listed = dest.list().await.unwrap();
    assert_eq!(
        listed.len(),
        2,
        "retention should keep only the 2 most recent"
    );
    let names: Vec<_> = listed.iter().map(|r| r.name.clone()).collect();
    // The new run is named with today's date, lexically newest. Retention
    // keeps it + the previous newest (snapshot-20260103T000000Z).
    assert!(names.iter().any(|n| n.starts_with("snapshot-2026")));

    unsafe { std::env::remove_var(PASSPHRASE_ENV) };
}
