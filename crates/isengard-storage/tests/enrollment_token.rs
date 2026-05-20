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

/// Imp-4 fix: cancel marks the token unusable without inventing a fake
/// host enrollment. Subsequent find_active and consume both reject it.
#[tokio::test]
async fn cancel_marks_cancelled_and_blocks_redemption() {
    let inv = fresh_inv().await;
    let h = hash("cancel-me");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    inv.cancel_enrollment_token(&h).await.unwrap();
    assert!(inv.find_active_token(&h).await.unwrap().is_none());
    let host_id = make_host(&inv).await;
    let err = inv
        .consume_enrollment_token(&h, host_id)
        .await
        .expect_err("cancelled token cannot be consumed");
    assert!(format!("{err:#}").contains("cancelled") || format!("{err:#}").contains("not found"));
}

#[tokio::test]
async fn cancel_already_consumed_errors() {
    let inv = fresh_inv().await;
    let h = hash("consume-then-cancel");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    let host_id = make_host(&inv).await;
    inv.consume_enrollment_token(&h, host_id).await.unwrap();
    let err = inv
        .cancel_enrollment_token(&h)
        .await
        .expect_err("already-consumed cannot be cancelled");
    assert!(format!("{err:#}").contains("consumed") || format!("{err:#}").contains("not found"));
}

#[tokio::test]
async fn cancel_twice_errors() {
    let inv = fresh_inv().await;
    let h = hash("twice-cancelled");
    inv.insert_enrollment_token(
        h.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    inv.cancel_enrollment_token(&h).await.unwrap();
    assert!(inv.cancel_enrollment_token(&h).await.is_err());
}

/// list_active_tokens must not surface cancelled tokens: they're as gone
/// as consumed/expired ones for the purposes of the dashboard's "pending
/// invitations" list.
#[tokio::test]
async fn list_active_tokens_skips_cancelled() {
    let inv = fresh_inv().await;
    let active = hash("still-active");
    let cancelled = hash("cancelled");
    inv.insert_enrollment_token(
        active.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    inv.insert_enrollment_token(
        cancelled.clone(),
        TokenRole::Agent,
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    inv.cancel_enrollment_token(&cancelled).await.unwrap();

    let listed = inv.list_active_tokens().await.unwrap();
    assert_eq!(listed.len(), 1, "only the still-active row should appear");
    assert_eq!(listed[0].token_hash, active);
}
