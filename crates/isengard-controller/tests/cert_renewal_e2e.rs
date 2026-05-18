//! Phase 14 Task 7: end-to-end test of the gRPC `RenewCert` handler.
//!
//! After the Bl-1 fix the request body carries no `host_id`; the controller
//! reads it authoritatively from the caller's client cert CN. This test
//! verifies the failure path when no peer cert is present in the request
//! extensions (i.e. the handler is called via the in-process service surface
//! that bypasses the TLS layer). The success path with a real client cert is
//! covered by `mtls_e2e.rs` and `crates/isengard-agent/tests/auth_e2e.rs`.

use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ControllerService;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_proto::pb::controller_server::Controller;
use isengard_proto::pb::{EnrollRequest, RenewCertRequest};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;

#[tokio::test]
async fn renew_cert_rejects_when_no_peer_cert_extension() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    let bare = enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();
    // Track G: redeem requires the packed `TK<bytes>.<fingerprint>` shape.
    let bytes_vec = data_encoding::BASE32_NOPAD
        .decode(bare.as_bytes())
        .expect("mint returns base32");
    let bytes: [u8; 32] = bytes_vec.as_slice().try_into().expect("32 bytes");
    let token = isengard_core::join_token::pack(&bytes, ca.root_cert_pem().as_bytes());

    let svc =
        ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone(), revocation)
            .await;

    // Enroll first to seed the controller with a host. Sanity-check the
    // resulting bundle even though we don't reuse the cert here.
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
    assert!(enroll_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));

    // Direct in-process call has no `TlsConnectInfo` extension on the
    // Request — handler must reject with Unauthenticated.
    let err = svc
        .renew_cert(tonic::Request::new(RenewCertRequest {}))
        .await
        .expect_err("renew_cert without peer cert must fail");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "got {err:?}");
}
