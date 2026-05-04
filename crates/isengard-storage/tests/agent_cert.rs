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
        fleet: "default".into(),
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
