use chrono::{Duration, Utc};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
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

fn hash(token: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).to_vec()
}

#[tokio::test]
async fn insert_then_find_returns_record() {
    let inv = fresh_inv().await;
    let h = hash("abc");
    let exp = Utc::now() + Duration::minutes(15);
    inv.insert_enrollment_token(h.clone(), TokenRole::Agent, exp)
        .await
        .unwrap();
    let rec = inv.find_active_token(&h).await.unwrap().expect("found");
    assert_eq!(rec.token_hash, h);
    assert_eq!(rec.role, TokenRole::Agent);
    assert!(rec.consumed_at.is_none());
}

#[tokio::test]
async fn find_active_skips_expired() {
    let inv = fresh_inv().await;
    let h = hash("expired");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() - Duration::seconds(1),
    )
    .await
    .unwrap();
    assert!(inv.find_active_token(&h).await.unwrap().is_none());
}

#[tokio::test]
async fn consume_marks_consumed_atomically() {
    let inv = fresh_inv().await;
    let h = hash("consume-me");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    let host_id = make_host(&inv).await;
    inv.consume_enrollment_token(&h, host_id).await.unwrap();
    let rec = inv.find_active_token(&h).await.unwrap();
    assert!(
        rec.is_none(),
        "consumed token should not be returned by find_active"
    );
}

#[tokio::test]
async fn consume_twice_errors() {
    let inv = fresh_inv().await;
    let h = hash("once");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    let host_id = make_host(&inv).await;
    inv.consume_enrollment_token(&h, host_id).await.unwrap();
    assert!(inv.consume_enrollment_token(&h, host_id).await.is_err());
}
