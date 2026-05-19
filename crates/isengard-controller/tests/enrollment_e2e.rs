//! End-to-end test of the gRPC `Enroll` handler going
//! through `EnrollmentService::redeem`. We exercise the [`Controller`] trait
//! impl directly (no transport) — that's enough to cover the wiring between
//! proto request/response, `EnrollmentService`, and the CA.

use std::sync::Arc;

use chrono::Duration;
use isengard_controller::ControllerService;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_server::Controller;
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;

#[tokio::test]
async fn enroll_with_valid_token_returns_cert_bundle() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    let bare = enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();
    // Redeem requires the packed `TK<bytes>.<fingerprint>` shape.
    let bytes_vec = data_encoding::BASE32_NOPAD
        .decode(bare.as_bytes())
        .expect("mint returns base32");
    let bytes: [u8; 32] = bytes_vec.as_slice().try_into().expect("32 bytes");
    let token = isengard_core::join_token::pack(&bytes, ca.root_cert_pem().as_bytes());

    let svc =
        ControllerService::new_for_test(inv.clone(), ca.clone(), enrollment.clone(), revocation)
            .await;
    let req = tonic::Request::new(EnrollRequest {
        token,
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1".into(),
    });
    let resp = svc.enroll(req).await.unwrap().into_inner();

    assert!(
        resp.agent_cert_pem.contains("BEGIN CERTIFICATE"),
        "agent cert PEM should be present"
    );
    assert!(
        resp.agent_key_pem.contains("BEGIN PRIVATE KEY"),
        "agent key PEM should be present"
    );
    assert!(
        resp.ca_root_pem.contains("BEGIN CERTIFICATE"),
        "CA root PEM should be present"
    );
    assert_eq!(resp.heartbeat_interval_secs, 10);
    assert_eq!(resp.host_id.len(), 16, "host_id must be 16-byte ULID");
}

#[tokio::test]
async fn enroll_with_invalid_token_returns_unauthenticated() {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    let svc = ControllerService::new_for_test(inv, ca, enrollment, revocation).await;
    let req = tonic::Request::new(EnrollRequest {
        token: "DOESNOTEXISTXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        hostname: "agent-1".into(),
        os: "linux".into(),
        version: "0.1".into(),
    });
    let err = svc.enroll(req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "got {err:?}");
}
