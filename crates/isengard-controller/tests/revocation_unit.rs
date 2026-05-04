//! Unit tests for `revocation::RevocationSet` (Task 4 of Phase 14).
//!
//! Covers initial load from inventory, runtime revocation, and the
//! persistent `revoke_agent` helper (active cert lookup + DB revoke +
//! in-memory set update).

use isengard_controller::revocation::{RevocationSet, revoke_agent};
use isengard_storage::Inventory;
use isengard_storage::agent_cert::AgentCert;
use isengard_storage::host::{EnrollHost, HostId};

async fn host_with_cert(inv: &Inventory, serial: Vec<u8>) -> HostId {
    let fp: String = serial.iter().map(|b| format!("{b:02x}")).collect();
    let id = inv
        .enroll_host(EnrollHost {
            fingerprint: format!("fp-{fp}"),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "27.0".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();
    inv.insert_agent_cert(AgentCert {
        serial,
        host_id: id,
        cert_pem: "p".into(),
        issued_at: chrono::Utc::now(),
        expires_at: chrono::Utc::now() + chrono::Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    })
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn load_from_inventory_picks_up_existing_revocations() {
    let inv = Inventory::open_in_memory().await.unwrap();
    host_with_cert(&inv, vec![1, 2, 3]).await;
    inv.revoke_cert(&[1, 2, 3], "test").await.unwrap();

    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    assert!(set.contains(&[1, 2, 3]));
    assert!(!set.contains(&[9, 9, 9]));
}

#[tokio::test]
async fn revoke_runtime_adds_to_set() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    set.revoke(vec![5, 5, 5]);
    assert!(set.contains(&[5, 5, 5]));
}

#[tokio::test]
async fn revoke_agent_persists_and_updates_set() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let host = host_with_cert(&inv, vec![7, 7, 7]).await;
    let set = RevocationSet::load_from_inventory(&inv).await.unwrap();
    assert!(!set.contains(&[7, 7, 7]));

    revoke_agent(&inv, &set, host, "decommission")
        .await
        .unwrap();

    assert!(set.contains(&[7, 7, 7]));
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
}
