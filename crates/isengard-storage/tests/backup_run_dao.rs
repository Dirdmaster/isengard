//! Backup_runs DAO unit tests.

use chrono::{Duration, Utc};
use isengard_storage::{BackupRunStatus, Inventory};

#[tokio::test]
async fn insert_creates_running_row() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let now = Utc::now();
    let id = inv.insert_backup_run(now).await.unwrap();
    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, id);
    assert_eq!(runs[0].status, BackupRunStatus::Running);
    assert!(runs[0].finished_at.is_none());
    assert!(runs[0].object_name.is_none());
    assert!(runs[0].size_bytes.is_none());
    assert!(runs[0].error.is_none());
}

#[tokio::test]
async fn finish_success_records_object_and_size() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let started = Utc::now();
    let id = inv.insert_backup_run(started).await.unwrap();
    let finished = started + Duration::seconds(5);
    inv.finish_backup_run_success(id, finished, "snapshot-20260506T120000Z.db.age", 4096)
        .await
        .unwrap();

    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs[0].status, BackupRunStatus::Success);
    assert_eq!(
        runs[0].object_name.as_deref(),
        Some("snapshot-20260506T120000Z.db.age")
    );
    assert_eq!(runs[0].size_bytes, Some(4096));
    assert!(runs[0].finished_at.is_some());
    assert!(runs[0].error.is_none());
}

#[tokio::test]
async fn finish_failed_records_error() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let id = inv.insert_backup_run(Utc::now()).await.unwrap();
    inv.finish_backup_run_failed(id, Utc::now(), "s3 upload failed: 403 forbidden")
        .await
        .unwrap();

    let runs = inv.list_backup_runs(10).await.unwrap();
    assert_eq!(runs[0].status, BackupRunStatus::Failed);
    assert_eq!(
        runs[0].error.as_deref(),
        Some("s3 upload failed: 403 forbidden")
    );
    assert!(runs[0].finished_at.is_some());
}

#[tokio::test]
async fn list_recent_returns_newest_first() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let t0 = Utc::now();
    let id_a = inv.insert_backup_run(t0).await.unwrap();
    let id_b = inv
        .insert_backup_run(t0 + Duration::seconds(1))
        .await
        .unwrap();
    let id_c = inv
        .insert_backup_run(t0 + Duration::seconds(2))
        .await
        .unwrap();

    let runs = inv.list_backup_runs(10).await.unwrap();
    let ids: Vec<_> = runs.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![id_c, id_b, id_a]);
}

#[tokio::test]
async fn list_recent_respects_limit() {
    let inv = Inventory::open_in_memory().await.unwrap();
    for _ in 0..5 {
        inv.insert_backup_run(Utc::now()).await.unwrap();
    }
    let runs = inv.list_backup_runs(2).await.unwrap();
    assert_eq!(runs.len(), 2);
}

#[tokio::test]
async fn status_round_trip() {
    assert_eq!(BackupRunStatus::Running.as_str(), "running");
    assert_eq!(BackupRunStatus::Success.as_str(), "success");
    assert_eq!(BackupRunStatus::Failed.as_str(), "failed");
    assert_eq!(
        BackupRunStatus::parse("running"),
        Some(BackupRunStatus::Running)
    );
    assert_eq!(
        BackupRunStatus::parse("success"),
        Some(BackupRunStatus::Success)
    );
    assert_eq!(
        BackupRunStatus::parse("failed"),
        Some(BackupRunStatus::Failed)
    );
    assert!(BackupRunStatus::parse("nope").is_none());
}

#[tokio::test]
async fn last_successful_returns_most_recent_success() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let t0 = Utc::now();

    let id_a = inv.insert_backup_run(t0).await.unwrap();
    inv.finish_backup_run_success(id_a, t0, "a.db.age", 100)
        .await
        .unwrap();

    let id_b = inv
        .insert_backup_run(t0 + Duration::seconds(1))
        .await
        .unwrap();
    inv.finish_backup_run_failed(id_b, t0 + Duration::seconds(1), "boom")
        .await
        .unwrap();

    let id_c = inv
        .insert_backup_run(t0 + Duration::seconds(2))
        .await
        .unwrap();
    inv.finish_backup_run_success(id_c, t0 + Duration::seconds(2), "c.db.age", 300)
        .await
        .unwrap();

    let last = inv.last_successful_backup_run().await.unwrap().unwrap();
    assert_eq!(last.id, id_c);
    assert_eq!(last.object_name.as_deref(), Some("c.db.age"));
}
