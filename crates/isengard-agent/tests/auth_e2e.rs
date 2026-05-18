//! Phase 14 Task 15: end-to-end auth lifecycle.
//!
//! In-process variant of the planned real-Docker test (see
//! `docs/superpowers/plans/2026-05-05-phase-14-auth-and-identity.md` Task 15
//! for the container-based version). This test exercises the same code paths
//! a real Docker e2e would — without the container build/pull cost — by
//! booting a real tonic Controller server in-process with mTLS + the
//! production [`CertAuthInterceptor`], then driving the agent's actual
//! [`isengard_agent::enroll::enroll`] + cert-store + mTLS endpoint code.
//!
//! Lifecycle covered:
//!   1. Operator mints an enrollment token via [`EnrollmentService::mint`].
//!   2. Agent calls the production `enroll::enroll` over the bootstrap
//!      channel (no client cert), pinning the controller's CA via the
//!      caller-supplied [`BootstrapTrust`].
//!   3. Agent persists the cert bundle via the production `cert_store`.
//!   4. Agent builds an mTLS endpoint and successfully calls `RenewCert`.
//!   5. Operator revokes the agent via [`revoke_agent`].
//!   6. Next `RenewCert` call fails with `Unauthenticated`.
//!
//! Hostname trick: the listener binds to a random loopback port and the
//! controller URL uses `https://localhost:PORT`. The server leaf's SAN is
//! `localhost`, so tonic's stock hostname verifier (no `domain_name`
//! override on the client side, matching the production agent) accepts it.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Duration;
use isengard_agent::cert_store;
use isengard_agent::enroll::{self, BootstrapTrust, HostInfo};
use isengard_controller::ControllerService;
use isengard_controller::auth::CertAuthInterceptor;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::{RevocationSet, revoke_agent};
use isengard_proto::pb::RenewCertRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, Server, ServerTlsConfig};

/// SAN baked into the server leaf. `localhost` resolves to the loopback IP
/// on every supported platform, so dialing `https://localhost:PORT` over
/// tonic's stock TLS config (no `domain_name` override) verifies cleanly.
const CONTROLLER_DNS: &str = "localhost";

struct ControllerHarness {
    url: String,
    inv: Arc<Inventory>,
    ca: Arc<Authority>,
    enrollment: Arc<EnrollmentService>,
    revocation: RevocationSet,
}

/// Boot a real tonic server with mTLS + the production interceptor on a
/// random loopback port. Mirrors the harness used by `mtls_e2e.rs`.
async fn boot_controller() -> ControllerHarness {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

    // Server identity: leaf signed by our CA, SAN = "localhost".
    // Imp-1: server certs require ServerAuth EKU via sign_server_leaf.
    let server_leaf = ca
        .sign_server_leaf(HostId::new(), CONTROLLER_DNS, Duration::days(30))
        .unwrap();
    let identity = Identity::from_pem(
        server_leaf.cert_pem.as_bytes(),
        server_leaf.key_pem.as_bytes(),
    );
    let ca_root = Certificate::from_pem(ca.root_cert_pem().as_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("https://{CONTROLLER_DNS}:{port}");

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
    // the test fires its first dial. Same pattern as `mtls_e2e.rs`.
    tokio::time::sleep(StdDuration::from_millis(100)).await;

    ControllerHarness {
        url,
        inv,
        ca,
        enrollment,
        revocation,
    }
}

#[tokio::test]
async fn full_auth_lifecycle_in_process() {
    // instant-acme 0.8 transitively pulls a newer rustls-platform-verifier
    // path that demands a process-level CryptoProvider. The bin targets
    // install one at startup; tests must do the same. Idempotent: a second
    // install is a no-op once one is set.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let harness = boot_controller().await;

    // --- 1. Mint enrollment token (operator side) ---------------------
    let token = harness
        .enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .expect("mint token");

    // --- 2. Agent enrolls via the production enroll::enroll ----------
    //   Pin the CA inline through `BootstrapTrust::ca_pem` — the same code
    //   path as `ISENGARD_CONTROLLER_CA_PEM` env var resolution. The path
    //   variant is covered by the unit test in `enroll.rs`.
    let trust = BootstrapTrust {
        ca_pem_path: None,
        ca_pem: Some(harness.ca.root_cert_pem().to_string()),
        verified_ca_pem: None,
    };
    let host_info = HostInfo {
        hostname: "agent-e2e".into(),
        os: "linux".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let outcome = enroll::enroll(&harness.url, &token, host_info, trust)
        .await
        .expect("enroll succeeds with pinned CA");

    // --- 3. Agent persists cert bundle via production cert_store ------
    let agent_state_dir = TempDir::new().unwrap();
    cert_store::save(agent_state_dir.path(), &outcome.bundle).expect("save bundle");
    assert!(
        cert_store::exists(agent_state_dir.path()),
        "cert files should be on disk after save"
    );

    // Sanity: controller-side state matches.
    let hosts = harness.inv.list_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1, "controller inventory has the new host");
    let host_row = hosts[0].clone();
    assert_eq!(host_row.hostname, "agent-e2e");
    let cert = harness
        .inv
        .active_cert_for_host(host_row.id)
        .await
        .unwrap()
        .expect("active cert row");
    assert!(
        cert.cert_pem.contains("BEGIN CERTIFICATE"),
        "stored leaf is valid PEM"
    );

    // --- 4. Agent builds mTLS endpoint + RenewCert succeeds ----------
    {
        let mut client = mtls_client(&harness, &outcome.bundle).await;
        // Bl-1 fix: RenewCertRequest no longer carries a host_id; the
        // controller reads it from the caller's client cert CN.
        let renew = client
            .renew_cert(RenewCertRequest {})
            .await
            .expect("RenewCert should succeed with a valid client cert")
            .into_inner();
        assert!(
            renew.agent_cert_pem.contains("BEGIN CERTIFICATE"),
            "renewed cert is valid PEM"
        );
        // `client` (and its channel) is dropped at end of scope — h2 may keep
        // the underlying connection alive briefly; step 6 forces a fresh dial.
    }

    // --- 5. Operator revokes EVERY active cert for the agent -------
    //   Imp-3 fix: a single revoke_agent call now revokes every active
    //   cert for the host (storage uses a single `UPDATE ... WHERE
    //   host_id = ? AND revoked_at IS NULL`). The successful RenewCert
    //   above minted a second active cert; before the fix the test
    //   needed a while loop to drain them. After the fix one call is
    //   enough.
    revoke_agent(
        &harness.inv,
        &harness.revocation,
        outcome.host_id,
        "phase-14-task-15-e2e",
    )
    .await
    .expect("revoke_agent");
    assert!(
        harness
            .inv
            .active_cert_for_host(outcome.host_id)
            .await
            .unwrap()
            .is_none(),
        "all active certs should be revoked after single revoke_agent",
    );

    // --- 6. Next mTLS call fails with Unauthenticated ----------------
    //   The revocation set is mutated in-place; on the next RPC the
    //   interceptor extracts the serial from the peer cert and finds it in
    //   the set. Build a fresh `Channel` so we force a brand-new TLS
    //   handshake (matches the production reconnect path; sync.rs rebuilds
    //   the channel on every reconnect).
    let mut client = mtls_client(&harness, &outcome.bundle).await;
    let err = client
        .renew_cert(RenewCertRequest {})
        .await
        .expect_err("revoked cert must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "expected Unauthenticated, got {err:?}",
    );
}

/// Build a fresh mTLS [`ControllerClient`] from a cert bundle. Re-built per
/// call to bypass tonic's connection cache when we want a fresh handshake
/// (specifically so the revocation check fires on the new connection).
///
/// This mirrors the channel construction in `isengard_agent::run_agent`
/// (`build_mtls_endpoint`) — same `ClientTlsConfig` shape, same identity
/// pieces — but reaches in directly so the test owns the lifetime.
async fn mtls_client(
    harness: &ControllerHarness,
    bundle: &cert_store::CertBundle,
) -> ControllerClient<Channel> {
    let identity = Identity::from_pem(bundle.cert_pem.as_bytes(), bundle.key_pem.as_bytes());
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(bundle.ca_pem.as_bytes()))
        .identity(identity);
    let channel = Channel::from_shared(harness.url.clone())
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    ControllerClient::new(channel)
}
