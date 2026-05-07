//! Phase 11a: LocalDestination round-trip tests.

use isengard_plugin_backup::destination::{BackupDestination, LocalDestination};
use tempfile::TempDir;

#[tokio::test]
async fn upload_list_download_delete_round_trip() {
    let dir = TempDir::new().unwrap();
    let dest = LocalDestination::new(dir.path(), "controllers/prod");

    dest.upload("snap-a.db.age", b"hello-a").await.unwrap();
    dest.upload("snap-b.db.age", b"hello-b").await.unwrap();

    let listed = dest.list().await.unwrap();
    let names: Vec<_> = listed.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["snap-a.db.age", "snap-b.db.age"]);

    let bytes = dest.download("snap-a.db.age").await.unwrap();
    assert_eq!(bytes, b"hello-a");

    dest.delete("snap-a.db.age").await.unwrap();
    let listed = dest.list().await.unwrap();
    let names: Vec<_> = listed.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["snap-b.db.age"]);
}

#[tokio::test]
async fn list_on_missing_dir_returns_empty() {
    let dir = TempDir::new().unwrap();
    let dest = LocalDestination::new(dir.path(), "no-such");
    let listed = dest.list().await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn delete_missing_object_is_ok() {
    let dir = TempDir::new().unwrap();
    let dest = LocalDestination::new(dir.path(), "");
    dest.delete("nope.db.age").await.unwrap();
}

#[tokio::test]
async fn invalid_object_name_is_rejected() {
    let dir = TempDir::new().unwrap();
    let dest = LocalDestination::new(dir.path(), "");
    let err = dest.upload("../escape.txt", b"x").await.unwrap_err();
    assert!(matches!(
        err,
        isengard_plugin_backup::destination::DestinationError::InvalidName(_)
    ));

    let err = dest.upload("with/slash", b"x").await.unwrap_err();
    assert!(matches!(
        err,
        isengard_plugin_backup::destination::DestinationError::InvalidName(_)
    ));
}

#[tokio::test]
async fn empty_prefix_writes_directly_under_root() {
    let dir = TempDir::new().unwrap();
    let dest = LocalDestination::new(dir.path(), "");
    dest.upload("flat.db.age", b"flat").await.unwrap();
    let path = dir.path().join("flat.db.age");
    assert!(path.exists());
    assert_eq!(std::fs::read(path).unwrap(), b"flat");
}
