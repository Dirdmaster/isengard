//! End-to-end: agent receives a packed token, calls `GetCaPem` against
//! a stub controller over skip-verify TLS, verifies the served CA's
//! fingerprint against the token, and surfaces either a verified PEM
//! or a fingerprint-mismatch error.
//!
//! The stub controller is a real tonic server bound on an ephemeral
//! loopback port, serving the agent over a self-signed cert. The
//! agent's skip-verify channel doesn't validate the cert; what closes
//! the trust loop is the SHA-256 fingerprint check on the served PEM.

use std::sync::Arc;

use isengard_core::join_token;
use isengard_proto::pb::controller_server::{Controller, ControllerServer};
use isengard_proto::pb::{
    AgentMessage, ControllerMessage, EnrollRequest, EnrollResponse, FetchSecretRequest,
    FetchSecretResponse, GetCaPemRequest, GetCaPemResponse, RenewCertRequest, RenewCertResponse,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

const FIXTURE_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nFIXTURE\n-----END CERTIFICATE-----\n";

/// Stub `Controller` that only implements `GetCaPem`. Other RPCs panic
/// if anything in this test suite accidentally hits them.
struct StubController {
    served_pem: Vec<u8>,
}

#[tonic::async_trait]
impl Controller for StubController {
    type SyncStream = ReceiverStream<Result<ControllerMessage, Status>>;

    async fn get_ca_pem(
        &self,
        _req: Request<GetCaPemRequest>,
    ) -> Result<Response<GetCaPemResponse>, Status> {
        Ok(Response::new(GetCaPemResponse {
            pem: self.served_pem.clone(),
        }))
    }

    async fn enroll(&self, _: Request<EnrollRequest>) -> Result<Response<EnrollResponse>, Status> {
        unimplemented!("not exercised in pre-enroll fingerprint tests")
    }

    async fn sync(
        &self,
        _: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SyncStream>, Status> {
        unimplemented!("not exercised in pre-enroll fingerprint tests")
    }

    async fn renew_cert(
        &self,
        _: Request<RenewCertRequest>,
    ) -> Result<Response<RenewCertResponse>, Status> {
        unimplemented!("not exercised in pre-enroll fingerprint tests")
    }

    async fn fetch_secret(
        &self,
        _: Request<FetchSecretRequest>,
    ) -> Result<Response<FetchSecretResponse>, Status> {
        unimplemented!("not exercised in pre-enroll fingerprint tests")
    }
}

/// Spin up the stub tonic server on an ephemeral loopback port,
/// returning the `https://...` URL the agent should dial. The server
/// runs for the lifetime of the test; the test's tokio runtime tearing
/// down at the end is what kills it.
async fn spawn_stub_controller(served_pem: Vec<u8>) -> String {
    // Process-wide rustls provider. Required because instant-acme +
    // rustls-platform-verifier panic if no default provider is set
    // before any TLS session is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Self-signed leaf for the listener. The agent's skip-verify
    // channel won't validate this; the test only needs to terminate TLS.
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("mint self-signed cert");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("https://{addr}");

    let svc = ControllerServer::new(StubController { served_pem });
    let tls = ServerTlsConfig::new().identity(identity);

    tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        Server::builder()
            .tls_config(tls)
            .expect("install server tls")
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .expect("stub controller serve");
    });

    // Tiny grace window so the listener is accepting before the test
    // dials it. The actual handshake retries inside tonic; the sleep
    // avoids a flake on the first connect.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    url
}

#[tokio::test]
async fn fingerprint_match_proceeds_to_enroll() {
    let url = spawn_stub_controller(FIXTURE_PEM.to_vec()).await;

    let bytes = [0x42u8; 32];
    let packed = join_token::pack(&bytes, FIXTURE_PEM);

    let ca_pem = isengard_agent::enroll::fetch_and_verify_ca(&url, &packed)
        .await
        .expect("fingerprint should match");
    assert_eq!(ca_pem, FIXTURE_PEM);
}

#[tokio::test]
async fn fingerprint_mismatch_hard_fails() {
    let served = b"-----BEGIN CERTIFICATE-----\nWRONG\n-----END CERTIFICATE-----\n".to_vec();
    let url = spawn_stub_controller(served).await;

    let bytes = [0x42u8; 32];
    let packed = join_token::pack(&bytes, FIXTURE_PEM);
    let err = isengard_agent::enroll::fetch_and_verify_ca(&url, &packed)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.to_lowercase().contains("fingerprint"), "{msg}");
    assert!(msg.to_lowercase().contains("mismatch"), "{msg}");
}

// Quiet the unused-imports lint on the Arc import that the trait impl
// would have used in a less stubby world; keep it for symmetry.
#[allow(dead_code)]
fn _phantom() -> Arc<()> {
    Arc::new(())
}
