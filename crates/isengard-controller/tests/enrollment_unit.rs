//! Unit tests for `EnrollmentService` (Task 3 of Phase 14).
//!
//! Covers token mint format, full redeem happy path, and the two refusal
//! cases (unknown token, double-redeem).

use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::{EnrollmentService, HostInfo};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;

async fn fixture() -> (Arc<Inventory>, EnrollmentService) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let svc = EnrollmentService::new(inv.clone(), ca);
    (inv, svc)
}

fn host_info() -> HostInfo {
    HostInfo {
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1.0".into(),
    }
}

#[tokio::test]
async fn mint_returns_base32_token_of_expected_length() {
    let (_, svc) = fixture().await;
    let token = svc
        .mint(TokenRole::Agent, Duration::minutes(15))
        .await
        .unwrap();
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "token must be uppercase base32 alphanum: {token}"
    );
    assert!(
        token.len() >= 50 && token.len() <= 56,
        "got length {}",
        token.len()
    );
}

#[tokio::test]
async fn redeem_valid_token_returns_signed_cert() {
    let (_, svc) = fixture().await;
    let token = svc
        .mint(TokenRole::Agent, Duration::minutes(15))
        .await
        .unwrap();
    let resp = svc.redeem(&token, host_info()).await.unwrap();

    assert!(resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(resp.agent_key_pem.contains("BEGIN PRIVATE KEY"));
    assert!(resp.ca_root_pem.contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn redeem_unknown_token_errors() {
    let (_, svc) = fixture().await;
    let err = svc
        .redeem("INVALID-TOKEN-XXXX", host_info())
        .await
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("token"));
}

#[tokio::test]
async fn redeem_twice_errors_second_time() {
    let (_, svc) = fixture().await;
    let token = svc
        .mint(TokenRole::Agent, Duration::minutes(15))
        .await
        .unwrap();
    svc.redeem(&token, host_info()).await.unwrap();
    let err = svc.redeem(&token, host_info()).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("token"));
}

/// Regression: two distinct hosts enrolling back-to-back must both land in
/// the hosts table. Pre-fix the controller passed `fingerprint: ""` for
/// every enrollment, and the second insert collided on the
/// `hosts.fingerprint TEXT NOT NULL UNIQUE` constraint, surfacing to the
/// agent as `status: Unauthenticated, message: "enroll host"`. The fix
/// derives the fingerprint from the leaf cert's SHA-256, which is unique
/// by construction (random 16-byte serial per leaf).
#[tokio::test]
async fn two_back_to_back_enrollments_get_distinct_fingerprints() {
    let (inv, svc) = fixture().await;

    let token_a = svc
        .mint(TokenRole::Agent, Duration::minutes(15))
        .await
        .unwrap();
    let token_b = svc
        .mint(TokenRole::Agent, Duration::minutes(15))
        .await
        .unwrap();

    let resp_a = svc
        .redeem(
            &token_a,
            HostInfo {
                hostname: "iso-fresh-1".into(),
                os: "linux".into(),
                version: "0.4.1".into(),
            },
        )
        .await
        .expect("first redeem must succeed");
    let resp_b = svc
        .redeem(
            &token_b,
            HostInfo {
                hostname: "iso-fresh-2".into(),
                os: "linux".into(),
                version: "0.4.1".into(),
            },
        )
        .await
        .expect("second redeem must succeed (was the P0 fingerprint bug)");

    assert_ne!(resp_a.host_id, resp_b.host_id, "host_ids must differ");

    let host_a = inv.get_host(resp_a.host_id).await.unwrap().unwrap();
    let host_b = inv.get_host(resp_b.host_id).await.unwrap().unwrap();

    assert_ne!(
        host_a.fingerprint, host_b.fingerprint,
        "fingerprints must differ between enrollments"
    );
    assert!(
        !host_a.fingerprint.is_empty() && !host_b.fingerprint.is_empty(),
        "fingerprints must not be empty (the original bug)"
    );
    assert_eq!(
        host_a.fingerprint.len(),
        64,
        "fingerprint should be sha256 hex (64 chars), got {:?}",
        host_a.fingerprint
    );
    assert!(
        host_a
            .fingerprint
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "fingerprint should be lowercase hex: {}",
        host_a.fingerprint
    );
}
