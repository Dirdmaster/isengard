//! Phase 14 Task 7: end-to-end test of the gRPC `RenewCert` handler.
//! Mints a token, enrolls a host (giving us the original cert), then calls
//! `renew_cert` and asserts the new cert is a different, valid PEM with an
//! RFC3339 expiry. Failure paths (unknown host, malformed host_id) are
//! covered by the unit-level checks in `service.rs` and `enrollment.rs`.

use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ControllerService;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{EnrollRequest, RenewCertRequest};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;

#[tokio::test]
async fn renew_cert_returns_fresh_valid_leaf_for_existing_host() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let token = enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();

    let svc = ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone()).await;

    // Enroll first to get the original cert + host_id.
    let enroll_resp = svc
        .enroll(tonic::Request::new(EnrollRequest {
            token,
            hostname: "agent-renew".into(),
            os: "linux".into(),
            version: "0.1".into(),
        }))
        .await
        .unwrap()
        .into_inner();

    let original_cert = enroll_resp.agent_cert_pem.clone();
    assert!(
        original_cert.contains("BEGIN CERTIFICATE"),
        "original cert should be valid PEM"
    );

    // Renew using the host_id we just got back.
    let renew_resp = svc
        .renew_cert(tonic::Request::new(RenewCertRequest {
            host_id: enroll_resp.host_id.clone(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(
        renew_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"),
        "renewed cert should be valid PEM"
    );
    assert!(
        renew_resp.agent_key_pem.contains("BEGIN PRIVATE KEY"),
        "renewed key should be valid PEM"
    );
    assert_ne!(
        renew_resp.agent_cert_pem, original_cert,
        "renewed cert must differ from the original (random serial + fresh keypair)"
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(&renew_resp.expires_at).is_ok(),
        "expires_at should be RFC3339, got {:?}",
        renew_resp.expires_at,
    );
}
