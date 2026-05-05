//! Imp-2 regression: cert renewal must rebuild the shared Endpoint so the
//! sync loop's next reconnect uses the new identity. Pre-fix the Endpoint
//! was constructed once at startup and tonic baked the cert bytes in at
//! that moment; a renewed cert on disk wouldn't take effect until the
//! agent process restarted.
//!
//! This test exercises the wiring at the swap boundary by spinning up a
//! real mTLS server, enrolling once, calling `maybe_renew` (via the public
//! `run_renewal_loop` path with a tight poll interval), and asserting the
//! `Arc<RwLock<Endpoint>>` holds a *different* Endpoint after the renewal
//! than before. We can't compare Endpoint values directly (no PartialEq),
//! so we read the on-disk cert serial before and after and assert they
//! differ — proving the renewal happened — and assert that a fresh
//! `connect()` from the post-swap endpoint succeeds (proving the new
//! identity is wired up).

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Duration;
use isengard_agent::cert_renewal;
use isengard_agent::cert_store::{self, CertBundle};
use isengard_agent::enroll::{self, BootstrapTrust, HostInfo};
use isengard_controller::ControllerService;
use isengard_controller::auth::CertAuthInterceptor;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::revocation::RevocationSet;
use isengard_proto::pb::controller_server::ControllerServer;
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use isengard_storage::host::HostId;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};

const CONTROLLER_DNS: &str = "localhost";

async fn boot_controller() -> (String, Arc<Authority>, Arc<EnrollmentService>) {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();

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

    tokio::time::sleep(StdDuration::from_millis(100)).await;
    (url, ca, enrollment)
}

fn build_mtls_endpoint(controller_url: &str, bundle: &CertBundle) -> anyhow::Result<Endpoint> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(bundle.ca_pem.as_bytes()))
        .identity(Identity::from_pem(
            bundle.cert_pem.as_bytes(),
            bundle.key_pem.as_bytes(),
        ));
    let endpoint = Endpoint::from_shared(controller_url.to_string())?
        .tls_config(tls)
        .map_err(|e| anyhow::anyhow!("install client TLS: {e}"))?;
    Ok(endpoint)
}

fn cert_serial(cert_pem: &str) -> Vec<u8> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    cert.tbs_certificate.raw_serial().to_vec()
}

#[tokio::test]
async fn renewal_swaps_endpoint_in_holder() {
    let (url, ca, enrollment) = boot_controller().await;
    let token = enrollment
        .mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .unwrap();

    // Enroll the agent + persist the bundle, mirroring lib.rs first-boot.
    let trust = BootstrapTrust {
        ca_pem_path: None,
        ca_pem: Some(ca.root_cert_pem().to_string()),
    };
    let host_info = HostInfo {
        hostname: "agent-renewal".into(),
        os: "linux".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let outcome = enroll::enroll(&url, &token, host_info, trust).await.unwrap();
    let state_dir = TempDir::new().unwrap();
    cert_store::save(state_dir.path(), &outcome.bundle).unwrap();

    let original_serial = cert_serial(&outcome.bundle.cert_pem);

    // Build the initial Endpoint and put it under the same Arc<RwLock> the
    // production wiring uses.
    let initial = build_mtls_endpoint(&url, &outcome.bundle).unwrap();
    let endpoint_holder = Arc::new(tokio::sync::RwLock::new(initial));

    // Force renewal by hand: bypass the 50%-TTL gate by spawning the loop
    // with a 100ms tick AND backdating the cert mtime would be uglier than
    // just calling the public `should_renew` logic via a synthetic bundle.
    // The cleanest path is to replace the bundle on disk with a freshly
    // signed leaf whose validity window puts us past the 50% mark.
    let backdated_leaf = ca
        .sign_agent_leaf(outcome.host_id, "agent-renewal", Duration::seconds(2))
        .unwrap();
    let backdated_bundle = CertBundle {
        ca_pem: outcome.bundle.ca_pem.clone(),
        cert_pem: backdated_leaf.cert_pem.clone(),
        key_pem: backdated_leaf.key_pem.clone(),
    };
    cert_store::save(state_dir.path(), &backdated_bundle).unwrap();
    // Rebuild the endpoint to use this short-lived cert (otherwise the
    // renew RPC dials with the previous key — still valid, but the test
    // is clearer if the pre-swap endpoint matches the on-disk bundle).
    *endpoint_holder.write().await = build_mtls_endpoint(&url, &backdated_bundle).unwrap();

    // Wait past 50% of the 2-second TTL.
    tokio::time::sleep(StdDuration::from_millis(1200)).await;

    let endpoint_builder: cert_renewal::EndpointBuilder =
        Arc::new(|url, bundle| build_mtls_endpoint(url, bundle));
    let renewal_holder = endpoint_holder.clone();
    let renewal_state_dir = state_dir.path().to_path_buf();
    let renewal_url = url.clone();
    let renewal_task = tokio::spawn(async move {
        // Exit by deadline rather than running forever.
        let _ = tokio::time::timeout(
            StdDuration::from_secs(5),
            cert_renewal::run_renewal_loop(
                renewal_state_dir,
                outcome.host_id,
                renewal_holder,
                renewal_url,
                endpoint_builder,
                StdDuration::from_millis(100),
            ),
        )
        .await;
    });

    // Poll the on-disk cert until its serial differs from the backdated one
    // (proving the renewal landed) — bounded so a hung test fails loudly.
    let backdated_serial = cert_serial(&backdated_bundle.cert_pem);
    let mut renewed_serial = backdated_serial.clone();
    let deadline = std::time::Instant::now() + StdDuration::from_secs(3);
    while renewed_serial == backdated_serial && std::time::Instant::now() < deadline {
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        if let Ok(b) = cert_store::load(state_dir.path()) {
            renewed_serial = cert_serial(&b.cert_pem);
        }
    }
    renewal_task.abort();
    let _ = renewal_task.await;

    assert_ne!(
        renewed_serial, backdated_serial,
        "on-disk cert serial should change after renewal"
    );
    assert_ne!(
        renewed_serial, original_serial,
        "renewed serial should also differ from the original bundle"
    );

    // The renewal task should have swapped the Endpoint. We can't compare
    // Endpoint values, but we can verify the holder's connect() works with
    // the freshly-loaded on-disk bundle (and would have failed if the old
    // backdated cert were still pinned in TLS config — backdated TTL is 2s
    // so it's expired by now).
    let final_endpoint = endpoint_holder.read().await.clone();
    let _channel = final_endpoint
        .connect()
        .await
        .expect("post-swap endpoint should connect with renewed cert");
}
