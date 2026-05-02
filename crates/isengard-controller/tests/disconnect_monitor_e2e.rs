//! Integration test: enroll a host, start the monitor with a tiny threshold +
//! fast polling, assert the agent.disconnect_long event reaches the bus and
//! lands in the journal.
//!
//! The freshly-enrolled host has `last_seen_at = None`; `effective_last_seen`
//! in the monitor falls back to `enrolled_at` (= "now" at enroll time). With a
//! 1-second threshold and a 1.5s sleep, the host ages past the threshold and
//! the monitor's next poll emits.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use isengard_controller::bus::EventBus;
use isengard_controller::disconnect_monitor::DisconnectMonitor;
use isengard_storage::{EnrollHost, Inventory, Journal};
use tempfile::tempdir;

#[tokio::test]
async fn disconnect_monitor_emits_for_stale_host() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("isengard.db");

    // Inventory + Journal share the SQLite file.
    let inventory = Arc::new(Inventory::open(&db).await.unwrap());
    let journal = Arc::new(Journal::open(&db).await.unwrap());
    let bus = Arc::new(EventBus::new());

    // Enroll a host so it exists in the inventory. last_seen_at stays None;
    // the monitor's effective_last_seen falls back to enrolled_at (now).
    let enroll = EnrollHost {
        fingerprint: "test-fp-stale".to_string(),
        hostname: "stale-host".to_string(),
        os: "linux".to_string(),
        arch: "x86_64".to_string(),
        agent_version: "0.1.0-test".to_string(),
        docker_version: "27.4.0".to_string(),
        fleet: "test".to_string(),
    };
    let host_id = inventory.enroll_host(enroll).await.unwrap();

    // Subscribe to bus before starting monitor so we don't miss the event.
    let mut rx = bus.subscribe();

    let monitor = Arc::new(DisconnectMonitor::new(
        inventory.clone(),
        journal.clone(),
        bus.clone(),
        1,   // 1-second threshold
        0.2, // 200ms poll
    ));
    let _handle = monitor.clone().start();

    // Wait for the host to age past the 1s threshold + at least one full poll
    // cycle (the monitor skips its first immediate tick).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Receive the event with a generous timeout to avoid races on slower CI.
    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    let event = received
        .expect("timed out waiting for agent.disconnect_long event")
        .expect("recv error on bus");

    assert_eq!(event.kind, "agent.disconnect_long");
    let expected_ulid: ulid::Ulid = host_id.into();
    assert_eq!(event.host_id, Some(expected_ulid));
    assert!(
        event.summary.contains("test-fp-stale"),
        "summary should mention the host fingerprint, got: {}",
        event.summary
    );

    // Verify it landed in the journal too.
    let rows = journal.list_recent(10).await.unwrap();
    assert!(
        rows.iter().any(|r| r.kind == "agent.disconnect_long"),
        "journal should contain the disconnect event; rows: {:?}",
        rows.iter().map(|r| &r.kind).collect::<Vec<_>>()
    );
}
