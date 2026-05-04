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
