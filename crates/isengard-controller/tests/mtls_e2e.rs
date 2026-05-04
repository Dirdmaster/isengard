//! Phase 14 Task 8: end-to-end mTLS test of the controller's gRPC server.
//!
//! Spins up a real tonic Server with mTLS (`client_auth_optional(true)` so
//! Enroll succeeds without a client cert), then exercises:
//!
//!   1. Bootstrap channel (no client cert) → Enroll → mTLS channel using the
//!      issued cert → RenewCert succeeds.
//!   2. Same as above but the cert is revoked between Enroll and RenewCert,
//!      and RenewCert returns `Unauthenticated`.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Duration;
use isengard_controller::ControllerService;
use isengard_controller::auth::CertAuthInterceptor;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{RevocationSet, revoke_agent};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_proto::pb::{EnrollRequest, RenewCertRequest};
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, Server, ServerTlsConfig};

const CONTROLLER_DNS: &str = "controller.local";

struct Boot {
    url: String,
    inv: Arc<Inventory>,
    ca: Arc<Authority>,
    enrollment: Arc<EnrollmentService>,
    revocation: RevocationSet,
}

async fn boot_controller() -> Boot {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    // Server identity: a leaf signed by our CA, with `controller.local` in
    // the SAN so the rustls-based client can verify the hostname.
    let server_leaf = ca
        .sign_agent_leaf(HostId::new(), CONTROLLER_DNS, Duration::days(30))
        .unwrap();
    let identity = Identity::from_pem(
        server_leaf.cert_pem.as_bytes(),
        server_leaf.key_pem.as_bytes(),
    );
    let ca_root = Certificate::from_pem(ca.root_cert_pem().as_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("https://{addr}");

    let svc = ControllerService::new_for_test(
        inv.clone(),
        ca.clone(),
        enrollment.clone(),
        revocation.clone(),
    )
    .await;
    let auth_layer = CertAuthInterceptor::new(revocation.clone(), ca.clone());

    let tls_config = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(ca_root)
        .client_auth_optional(true);

    tokio::spawn(async move {
        Server::builder()
            .tls_config(tls_config)
            .unwrap()
            .layer(auth_layer)
            .add_service(ControllerServer::new(svc))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Tiny delay so the spawned server is ready to accept connections before
    // the test fires its first dial.
    tokio::time::sleep(StdDuration::from_millis(100)).await;

    Boot {
        url,
        inv,
        ca,
        enrollment,
        revocation,
    }
}

fn bootstrap_tls(ca_root_pem: &str) -> ClientTlsConfig {
    ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_root_pem.as_bytes()))
        .domain_name(CONTROLLER_DNS)
}

fn mtls(ca_root_pem: &str, cert_pem: &str, key_pem: &str) -> ClientTlsConfig {
    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_root_pem.as_bytes()))
        .identity(identity)
        .domain_name(CONTROLLER_DNS)
}

#[tokio::test]
async fn enroll_then_mtls_heartbeat_succeeds() {
    let boot = boot_controller().await;
    let token = boot
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();

    // Phase 1: bootstrap channel (no client cert) → enroll. The interceptor
    // bypasses the cert check for the Enroll method, so this succeeds.
    let channel = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(bootstrap_tls(boot.ca.root_cert_pem()))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel);
    let resp = client
        .enroll(EnrollRequest {
            token,
            hostname: "agent-1".into(),
            os: "linux".into(),
            version: "0.1".into(),
        })
        .await
        .unwrap()
        .into_inner();

    // Phase 2: mTLS channel using the issued cert → RenewCert. This RPC is
    // NOT in PUBLIC_METHODS, so the interceptor enforces cert presence + the
    // revocation check before dispatching.
    let channel = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(mtls(
            &resp.ca_root_pem,
            &resp.agent_cert_pem,
            &resp.agent_key_pem,
        ))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel);

    let renew_resp = client
        .renew_cert(RenewCertRequest {
            host_id: resp.host_id.clone(),
        })
        .await
        .expect("RenewCert should succeed with a valid client cert")
        .into_inner();
    assert!(renew_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn revoked_cert_rejected() {
    let boot = boot_controller().await;
    let token = boot
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();

    // Bootstrap → enroll
    let channel = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(bootstrap_tls(boot.ca.root_cert_pem()))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel);
    let resp = client
        .enroll(EnrollRequest {
            token,
            hostname: "agent-revoke".into(),
            os: "linux".into(),
            version: "0.1".into(),
        })
        .await
        .unwrap()
        .into_inner();

    // Revoke the freshly-issued cert. The in-memory set is shared with the
    // running server (we passed the same Arc into both), so the next RPC sees
    // it immediately.
    let host_id = HostId::from_db_bytes(resp.host_id.clone()).unwrap();
    revoke_agent(&boot.inv, &boot.revocation, host_id, "test")
        .await
        .unwrap();

    // Attempt mTLS with the revoked cert. The interceptor should reject with
    // Unauthenticated before dispatching to the handler.
    let channel = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(mtls(
            &resp.ca_root_pem,
            &resp.agent_cert_pem,
            &resp.agent_key_pem,
        ))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel);

    let err = client
        .renew_cert(RenewCertRequest {
            host_id: resp.host_id,
        })
        .await
        .expect_err("RenewCert with a revoked cert must fail");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "got {err:?}");
}
