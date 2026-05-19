//! End-to-end restore integration tests.
//!
//! Each test sets up a controller-side state (Inventory + on-disk DB),
//! runs a backup via the 11A pipeline (snapshot -> encrypt -> upload to a
//! local destination), then runs a restore via the 11B pipeline (download ->
//! decrypt -> validate -> swap -> migrate) and checks the outcome.

use std::str::FromStr;
use std::sync::Arc;

use isengard_plugin_backup::config::{BackupConfig, DestinationConfig};
use isengard_plugin_backup::destination::{BackupDestination, LocalDestination};
use isengard_plugin_backup::encrypt::passphrase_fingerprint;
use isengard_plugin_backup::restore::{RestoreError, restore_from_destination};
use isengard_plugin_backup::runner::{BackupRunner, PASSPHRASE_ENV};
use isengard_storage::{BackupRunStatus, Inventory, RestoreRunStatus};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tempfile::TempDir;

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

async fn open_disk_pool(path: &std::path::Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePool::connect_with(opts).await.unwrap()
}

/// Set up a freshly-seeded controller, run a backup against a local
/// destination, and return (TempDir, db_path, dest_root, prefix, passphrase,
/// object_name).
async fn seed_and_backup(
    pass: &str,
    seed_value: &str,
) -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
    String,
) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let dest_root = dir.path().join("backups");
    let prefix = "controllers/test".to_string();

    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());
    inv.set_setting("seed.key", &serde_json::json!(seed_value))
        .await
        .unwrap();

    let cfg = BackupConfig {
        enabled: true,
        destination: DestinationConfig::Local {
            root: dest_root.to_string_lossy().to_string(),
            prefix: prefix.clone(),
        },
        interval_secs: 86_400,
        retention_keep: 14,
        passphrase_fingerprint: passphrase_fingerprint(pass),
    };
    cfg.save(&inv).await.unwrap();

    unsafe { std::env::set_var(PASSPHRASE_ENV, pass) };

    let pool = open_disk_pool(&db_path).await;
    let runner = BackupRunner::new(inv.clone(), pool, db_path.clone());
    runner.run_once().await.unwrap();
    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs[0].status, BackupRunStatus::Success);
    let object_name = runs[0].object_name.as_ref().unwrap().clone();

    unsafe { std::env::remove_var(PASSPHRASE_ENV) };

    (dir, db_path, dest_root, prefix, object_name)
}

#[tokio::test]
async fn restore_round_trip_recovers_seeded_row() {
    let _guard = env_lock().await;
    let pass = "round-trip-pass-1";
    let (dir, db_path, dest_root, prefix, object_name) =
        seed_and_backup(pass, "hello-from-backup").await;

    // Mutate the live DB after the backup (so we know the restore actually
    // replaced bytes).
    {
        let inv = Inventory::open(&db_path).await.unwrap();
        inv.set_setting("seed.key", &serde_json::json!("post-backup-mutation"))
            .await
            .unwrap();
    }

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let outcome = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        pass,
        false,
    )
    .await
    .unwrap();

    assert!(!outcome.dry_run);
    assert!(outcome.bytes_restored > 0);
    assert!(!outcome.previous_db_backup_path.is_empty());
    assert!(std::path::Path::new(&outcome.previous_db_backup_path).exists());

    // Verify the seeded value is now what was backed up.
    let inv2 = Inventory::open(&db_path).await.unwrap();
    let v = inv2.get_setting("seed.key").await.unwrap();
    assert_eq!(v, Some(serde_json::json!("hello-from-backup")));

    // Verify the run row landed as success.
    let runs = inv2.list_restore_runs(10).await.unwrap();
    assert_eq!(runs[0].status, RestoreRunStatus::Success);
    assert_eq!(runs[0].source_object, object_name);

    drop(dir);
}

#[tokio::test]
async fn dry_run_validates_without_swapping() {
    let _guard = env_lock().await;
    let pass = "dry-run-pass-2";
    let (dir, db_path, dest_root, prefix, object_name) = seed_and_backup(pass, "live-bytes").await;

    {
        let inv = Inventory::open(&db_path).await.unwrap();
        inv.set_setting("seed.key", &serde_json::json!("untouched"))
            .await
            .unwrap();
    }

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let outcome = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        pass,
        true,
    )
    .await
    .unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.bytes_restored, 0);
    assert!(outcome.previous_db_backup_path.is_empty());

    // The live DB still has the post-backup value (no swap happened).
    let inv2 = Inventory::open(&db_path).await.unwrap();
    let v = inv2.get_setting("seed.key").await.unwrap();
    assert_eq!(v, Some(serde_json::json!("untouched")));

    // No `.bak.` file exists because the swap was skipped.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".bak."))
        .collect();
    assert!(
        entries.is_empty(),
        "no .bak.<ts> on dry-run, found {entries:?}"
    );

    drop(dir);
}

#[tokio::test]
async fn wrong_passphrase_returns_decrypt_error() {
    let _guard = env_lock().await;
    let pass = "right-pass";
    let (dir, db_path, dest_root, prefix, object_name) = seed_and_backup(pass, "anything").await;

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let err = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        "WRONG-PASS",
        false,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, RestoreError::Decrypt(_)), "got {err:?}");

    // Run row recorded as failed.
    let inv2 = Inventory::open(&db_path).await.unwrap();
    let runs = inv2.list_restore_runs(10).await.unwrap();
    assert_eq!(runs[0].status, RestoreRunStatus::Failed);

    drop(dir);
}

#[tokio::test]
async fn missing_object_returns_destination_error() {
    let _guard = env_lock().await;
    let pass = "missing-pass";
    let (dir, db_path, dest_root, prefix, _object_name) = seed_and_backup(pass, "anything").await;

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let err = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        "snapshot-does-not-exist.db.age",
        None,
        pass,
        false,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, RestoreError::Destination(_)), "got {err:?}");

    drop(dir);
}

#[tokio::test]
async fn empty_passphrase_rejected() {
    let _guard = env_lock().await;
    let pass = "valid";
    let (dir, db_path, dest_root, prefix, object_name) = seed_and_backup(pass, "anything").await;

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let err = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        "",
        false,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, RestoreError::EmptyPassphrase), "got {err:?}");
    drop(dir);
}

#[tokio::test]
async fn garbage_bytes_after_decrypt_rejected_as_invalid_snapshot() {
    let _guard = env_lock().await;
    let pass = "validation-pass";
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("isengard.db");
    let dest_root = dir.path().join("backups");
    let prefix = "x".to_string();

    // Set up a real DB so the restore_runs row can be inserted.
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    // Encrypt a non-DB blob with the same passphrase so decryption succeeds
    // but SQLite validation fails.
    let cipher = isengard_plugin_backup::encrypt::encrypt_with_passphrase(
        b"this is not a sqlite database",
        pass,
    )
    .unwrap();

    let dest = LocalDestination::new(&dest_root, &prefix);
    dest.upload("snapshot-bogus.db.age", &cipher).await.unwrap();

    let err = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        "snapshot-bogus.db.age",
        None,
        pass,
        false,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, RestoreError::InvalidSnapshot(_)),
        "got {err:?}"
    );

    drop(dir);
}

#[tokio::test]
async fn swap_preserves_previous_db_at_bak_path() {
    let _guard = env_lock().await;
    let pass = "swap-preserve-pass";
    let (dir, db_path, dest_root, prefix, object_name) =
        seed_and_backup(pass, "round-1-value").await;

    // Mutate the live DB so the .bak.<ts> file's bytes differ from the
    // snapshot's bytes.
    {
        let inv = Inventory::open(&db_path).await.unwrap();
        inv.set_setting("seed.key", &serde_json::json!("MUTATED"))
            .await
            .unwrap();
    }

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    let outcome = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        pass,
        false,
    )
    .await
    .unwrap();

    let bak_path = std::path::PathBuf::from(&outcome.previous_db_backup_path);
    assert!(bak_path.exists(), "expected .bak.<ts> at {bak_path:?}");

    // The .bak.<ts> file is a working SQLite DB holding the pre-restore
    // (mutated) value. Open it and check.
    let bak_inv = Inventory::open(&bak_path).await.unwrap();
    let v = bak_inv.get_setting("seed.key").await.unwrap();
    assert_eq!(
        v,
        Some(serde_json::json!("MUTATED")),
        ".bak.<ts> should hold the pre-restore (mutated) value"
    );

    // And the live DB now holds the snapshot value.
    let live_inv = Inventory::open(&db_path).await.unwrap();
    let v_live = live_inv.get_setting("seed.key").await.unwrap();
    assert_eq!(v_live, Some(serde_json::json!("round-1-value")));

    drop(dir);
}

#[tokio::test]
async fn restore_runs_records_failed_attempt_before_success() {
    let _guard = env_lock().await;
    let pass = "list-pass";
    let (dir, db_path, dest_root, prefix, object_name) = seed_and_backup(pass, "init").await;

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    // First attempt: wrong passphrase. The original DB still holds the
    // running -> failed transition.
    let _ = restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        "wrong",
        false,
    )
    .await;

    // The original DB has both the failed attempt row and the seeded value.
    let inv_before = Inventory::open(&db_path).await.unwrap();
    let runs_before = inv_before.list_restore_runs(10).await.unwrap();
    assert_eq!(runs_before.len(), 1);
    assert_eq!(runs_before[0].status, RestoreRunStatus::Failed);
    drop(inv_before);

    // Now swap with a real restore. After this the live DB IS the snapshot,
    // which had no restore_runs rows. We insert a fresh success row in the
    // new file post-swap, so the live DB ends up with exactly one row.
    let inv2 = Arc::new(Inventory::open(&db_path).await.unwrap());
    restore_from_destination(
        &inv2,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        pass,
        false,
    )
    .await
    .unwrap();

    let inv3 = Inventory::open(&db_path).await.unwrap();
    let runs_after = inv3.list_restore_runs(10).await.unwrap();
    assert_eq!(
        runs_after.len(),
        1,
        "post-restore: only the success row exists (snapshot had none)"
    );
    assert_eq!(runs_after[0].status, RestoreRunStatus::Success);

    drop(dir);
}

#[tokio::test]
async fn migrations_apply_after_restore() {
    // Restoring a snapshot taken with current migrations and re-opening should
    // succeed (migrations are idempotent). This exercises the "open fresh
    // Inventory after swap" path.
    let _guard = env_lock().await;
    let pass = "migrate-pass";
    let (dir, db_path, dest_root, prefix, object_name) = seed_and_backup(pass, "migrate-me").await;

    let dest = LocalDestination::new(&dest_root, &prefix);
    let inv = Arc::new(Inventory::open(&db_path).await.unwrap());

    restore_from_destination(
        &inv,
        &db_path,
        &dest as &dyn BackupDestination,
        &object_name,
        None,
        pass,
        false,
    )
    .await
    .unwrap();

    // Open the restored file again. If migrations re-applied cleanly this
    // works; if not, sqlx panics on schema mismatch.
    let inv2 = Inventory::open(&db_path).await.unwrap();
    let v = inv2.get_setting("seed.key").await.unwrap();
    assert_eq!(v, Some(serde_json::json!("migrate-me")));

    drop(dir);
}
