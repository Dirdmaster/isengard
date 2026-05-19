//! End-to-end mTLS test of the controller's gRPC server.
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
    // instant-acme 0.8 transitively pulls in a rustls path that demands an
    // explicit process-level CryptoProvider. Idempotent: install fails (Err)
    // if a provider is already set, which we ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    // Server identity: a leaf signed by our CA, with `controller.local` in
    // the SAN so the rustls-based client can verify the hostname.
    // Imp-1: server certs go through sign_server_leaf (ServerAuth EKU);
    // sign_agent_leaf is ClientAuth-only post-Imp-1.
    let server_leaf = ca
        .sign_server_leaf(HostId::new(), CONTROLLER_DNS, Duration::days(30))
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

/// Redeem requires the packed `TK<bytes>.<fingerprint>` shape;
/// wrap a bare-base32 token (the mint output) with the harness's CA.
fn pack(bare: &str, ca_pem: &str) -> String {
    let bytes_vec = data_encoding::BASE32_NOPAD
        .decode(bare.as_bytes())
        .expect("mint returns base32");
    let bytes: [u8; 32] = bytes_vec.as_slice().try_into().expect("32 bytes");
    isengard_core::join_token::pack(&bytes, ca_pem.as_bytes())
}

#[tokio::test]
async fn enroll_then_mtls_heartbeat_succeeds() {
    let boot = boot_controller().await;
    let bare = boot
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();
    let token = pack(&bare, boot.ca.root_cert_pem());

    // Bootstrap channel (no client cert) → enroll. The interceptor
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

    // mTLS channel using the issued cert → RenewCert. This RPC is
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
        .renew_cert(RenewCertRequest {})
        .await
        .expect("RenewCert should succeed with a valid client cert")
        .into_inner();
    assert!(renew_resp.agent_cert_pem.contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn revoked_cert_rejected() {
    let boot = boot_controller().await;
    let bare = boot
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();
    let token = pack(&bare, boot.ca.root_cert_pem());

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
        .renew_cert(RenewCertRequest {})
        .await
        .expect_err("RenewCert with a revoked cert must fail");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "got {err:?}");
}

/// Bl-1 regression test: agent A connects with its own cert, calls
/// RenewCert. With the fix, RenewCertRequest carries no host_id and the
/// controller authoritatively reads the host_id from agent A's cert CN —
/// regardless of any client claims — so the renewed cert is minted for A.
/// We verify by comparing the resulting cert's CN against agent A's host_id.
///
/// Pre-fix the attacker would have stuffed B's host_id in the request body
/// to make the controller mint a cert for B; verifying the renewed cert's
/// CN equals the caller's cert CN proves the controller now sources the
/// host_id from transport authentication only.
#[tokio::test]
async fn renew_cert_uses_caller_cert_cn_not_request_body() {
    let boot = boot_controller().await;

    let bare = boot
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();
    let token = pack(&bare, boot.ca.root_cert_pem());

    // Bootstrap → enroll agent A.
    let channel = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(bootstrap_tls(boot.ca.root_cert_pem()))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel);
    let resp_a = client
        .enroll(EnrollRequest {
            token,
            hostname: "agent-a".into(),
            os: "linux".into(),
            version: "0.1".into(),
        })
        .await
        .unwrap()
        .into_inner();

    let host_id_a = HostId::from_db_bytes(resp_a.host_id.clone()).unwrap();

    // Mint a fake "B" host_id the attacker would have liked to hand the
    // controller. It's never enrolled and the cert never sees it; the test
    // just proves the controller ignores client-supplied host_id entirely.
    let host_id_b = HostId::new();
    assert_ne!(host_id_a, host_id_b);

    // Agent A dials with A's cert and calls RenewCert. The request body has
    // no host_id at all post-fix; the controller is forced to use the cert.
    let channel_a = Channel::from_shared(boot.url.clone())
        .unwrap()
        .tls_config(mtls(
            &resp_a.ca_root_pem,
            &resp_a.agent_cert_pem,
            &resp_a.agent_key_pem,
        ))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControllerClient::new(channel_a);
    let renewed = client
        .renew_cert(RenewCertRequest {})
        .await
        .expect("RenewCert with A's cert should succeed")
        .into_inner();

    // The renewed cert's CN must equal A's host_id (proving the controller
    // sourced the identity from the cert, not from the body).
    let (_, pem) = x509_parser::pem::parse_x509_pem(renewed.agent_cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .expect("renewed leaf has CN")
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        cn,
        host_id_a.to_string(),
        "renewed cert was minted for the caller's cert CN identity"
    );
    assert_ne!(cn, host_id_b.to_string());

    // Sanity: the new cert is also persisted under host A.
    let active = boot
        .inv
        .active_cert_for_host(host_id_a)
        .await
        .unwrap()
        .expect("A has an active cert after renewal");
    assert!(active.cert_pem.contains("BEGIN CERTIFICATE"));
}
