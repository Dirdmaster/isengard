use chrono::{Duration, Utc};
use isengard_storage::Inventory;
use isengard_storage::agent_cert::AgentCert;
use isengard_storage::host::{EnrollHost, HostId};

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

async fn make_host(inv: &Inventory) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: "fp".into(),
        hostname: "h1".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1".into(),
        docker_version: "27".into(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn insert_then_active_lookup_returns_cert() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    let cert = AgentCert {
        serial: vec![1, 2, 3, 4],
        host_id: host,
        cert_pem: "cert-pem".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    };
    inv.insert_agent_cert(cert.clone()).await.unwrap();
    let active = inv
        .active_cert_for_host(host)
        .await
        .unwrap()
        .expect("present");
    assert_eq!(active.serial, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn revoke_marks_revoked_and_active_returns_none() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    let serial = vec![9, 9, 9];
    inv.insert_agent_cert(AgentCert {
        serial: serial.clone(),
        host_id: host,
        cert_pem: "p".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    })
    .await
    .unwrap();
    inv.revoke_cert(&serial, "test").await.unwrap();
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
}

/// Imp-3 fix: revoke_all_active_for_host wipes every active cert in one
/// call so an operator's "kick this host off" intent doesn't depend on
/// how many renewals the agent has done.
#[tokio::test]
async fn revoke_all_active_for_host_no_active_returns_empty() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    let revoked = inv
        .revoke_all_active_for_host(host, "decommission")
        .await
        .unwrap();
    assert!(revoked.is_empty(), "no active certs → no serials returned");
}

#[tokio::test]
async fn revoke_all_active_for_host_single_active_revokes_it() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    inv.insert_agent_cert(AgentCert {
        serial: vec![10],
        host_id: host,
        cert_pem: "p".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    })
    .await
    .unwrap();
    let revoked = inv
        .revoke_all_active_for_host(host, "decommission")
        .await
        .unwrap();
    assert_eq!(revoked, vec![vec![10u8]]);
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
}

#[tokio::test]
async fn revoke_all_active_for_host_revokes_all_active_certs() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    for s in [vec![1u8], vec![2u8], vec![3u8]] {
        inv.insert_agent_cert(AgentCert {
            serial: s,
            host_id: host,
            cert_pem: "p".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(30),
            revoked_at: None,
            revoke_reason: None,
        })
        .await
        .unwrap();
    }
    let mut revoked = inv
        .revoke_all_active_for_host(host, "decommission")
        .await
        .unwrap();
    revoked.sort();
    assert_eq!(revoked, vec![vec![1u8], vec![2u8], vec![3u8]]);
    assert!(inv.active_cert_for_host(host).await.unwrap().is_none());
    let mut after_serials = inv.revoked_serials().await.unwrap();
    after_serials.sort();
    assert_eq!(after_serials, vec![vec![1u8], vec![2u8], vec![3u8]]);
}

#[tokio::test]
async fn revoke_all_active_for_host_skips_already_revoked() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    inv.insert_agent_cert(AgentCert {
        serial: vec![1],
        host_id: host,
        cert_pem: "p".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    })
    .await
    .unwrap();
    inv.insert_agent_cert(AgentCert {
        serial: vec![2],
        host_id: host,
        cert_pem: "p".into(),
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        revoked_at: None,
        revoke_reason: None,
    })
    .await
    .unwrap();
    inv.revoke_cert(&[1], "earlier").await.unwrap();

    let revoked = inv.revoke_all_active_for_host(host, "later").await.unwrap();
    assert_eq!(revoked, vec![vec![2u8]]);
}

#[tokio::test]
async fn revoked_serials_returns_all_revoked() {
    let inv = fresh_inv().await;
    let host = make_host(&inv).await;
    for s in [vec![1u8], vec![2u8], vec![3u8]] {
        inv.insert_agent_cert(AgentCert {
            serial: s.clone(),
            host_id: host,
            cert_pem: "p".into(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(30),
            revoked_at: None,
            revoke_reason: None,
        })
        .await
        .unwrap();
    }
    inv.revoke_cert(&[1], "r").await.unwrap();
    inv.revoke_cert(&[3], "r").await.unwrap();
    let mut got = inv.revoked_serials().await.unwrap();
    got.sort();
    assert_eq!(got, vec![vec![1u8], vec![3u8]]);
}
