//! End-to-end: agent receives a packed token, fetches the CA from a
//! mocked controller via skip-verify, verifies fingerprint, then would
//! call Enroll. The Enroll call itself stays stubbed (covered by
//! existing auth_e2e tests); this test only locks the pre-enroll
//! fingerprint flow.

use isengard_core::join_token;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nFIXTURE\n-----END CERTIFICATE-----\n";

#[tokio::test]
async fn fingerprint_match_proceeds_to_enroll() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ca/pem"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(FIXTURE_PEM))
        .expect(1)
        .mount(&server)
        .await;

    let bytes = [0x42u8; 32];
    let packed = join_token::pack(&bytes, FIXTURE_PEM);
    let controller_url = server.uri();

    let ca_pem = isengard_agent::enroll::fetch_and_verify_ca(&controller_url, &packed)
        .await
        .expect("fingerprint should match");
    assert_eq!(ca_pem, FIXTURE_PEM);
}

#[tokio::test]
async fn fingerprint_mismatch_hard_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ca/pem"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            b"-----BEGIN CERTIFICATE-----\nWRONG\n-----END CERTIFICATE-----\n" as &[u8],
        ))
        .mount(&server)
        .await;

    let bytes = [0x42u8; 32];
    let packed = join_token::pack(&bytes, FIXTURE_PEM);
    let err = isengard_agent::enroll::fetch_and_verify_ca(&server.uri(), &packed)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.to_lowercase().contains("fingerprint"));
    assert!(msg.to_lowercase().contains("mismatch"));
}
