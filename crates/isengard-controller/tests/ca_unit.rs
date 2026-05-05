//! Unit tests for `ca::Authority`. See spec
//! docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md.

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_storage::Inventory;
use isengard_storage::host::HostId;

#[tokio::test]
async fn load_or_init_creates_then_persists() {
    let inv = Inventory::open_in_memory().await.unwrap();

    let auth1 = Authority::load_or_init(&inv).await.unwrap();
    let cert1 = auth1.root_cert_pem().to_string();

    let auth2 = Authority::load_or_init(&inv).await.unwrap();
    assert_eq!(
        auth2.root_cert_pem(),
        cert1,
        "second load should reuse persisted CA"
    );
}

#[tokio::test]
async fn sign_agent_leaf_chains_to_root() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let host_id = HostId::new();
    let leaf = auth
        .sign_agent_leaf(host_id, "agent-host", Duration::days(30))
        .unwrap();

    assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(leaf.key_pem.contains("BEGIN PRIVATE KEY"));
    assert_eq!(leaf.serial.len(), 16);
    assert!(leaf.expires_at > chrono::Utc::now());

    // Verify the leaf chains to the CA. Use x509-parser to load both, then
    // verify the leaf's signature against the CA's public key.
    let (_, root) = x509_parser::pem::parse_x509_pem(auth.root_cert_pem().as_bytes()).unwrap();
    let root_cert = root.parse_x509().unwrap();
    let (_, leaf_pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let leaf_cert = leaf_pem.parse_x509().unwrap();
    leaf_cert
        .verify_signature(Some(root_cert.public_key()))
        .expect("leaf chains to root");
}

#[tokio::test]
async fn sign_agent_leaf_includes_hostname_san() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let leaf = auth
        .sign_agent_leaf(HostId::new(), "my-agent.example.com", Duration::days(30))
        .unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let san = cert
        .subject_alternative_name()
        .unwrap()
        .expect("SAN present");
    let dns_names: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert!(
        dns_names.contains(&"my-agent.example.com"),
        "got {dns_names:?}"
    );
}

/// Imp-1 regression: agent leaves must NOT carry ServerAuth EKU. An agent
/// could otherwise enroll with a hostname like "controller.local" and
/// receive a cert other peers would trust as the controller. Server certs
/// are minted via [`Authority::sign_server_leaf`] instead.
#[tokio::test]
async fn sign_agent_leaf_omits_server_auth_eku() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let leaf = auth
        .sign_agent_leaf(HostId::new(), "agent-host", Duration::days(30))
        .unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let eku = cert
        .extended_key_usage()
        .unwrap()
        .expect("EKU extension present");
    assert!(eku.value.client_auth, "agent leaf has ClientAuth");
    assert!(
        !eku.value.server_auth,
        "agent leaf must NOT have ServerAuth"
    );
}

#[tokio::test]
async fn sign_server_leaf_has_both_ekus() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let leaf = auth
        .sign_server_leaf(HostId::new(), "controller.local", Duration::days(30))
        .unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(leaf.cert_pem.as_bytes()).unwrap();
    let cert = pem.parse_x509().unwrap();
    let eku = cert
        .extended_key_usage()
        .unwrap()
        .expect("EKU extension present");
    assert!(eku.value.client_auth, "server leaf has ClientAuth");
    assert!(eku.value.server_auth, "server leaf has ServerAuth");
}

#[tokio::test]
async fn sign_agent_leaf_rejects_overly_long_hostname() {
    let inv = Inventory::open_in_memory().await.unwrap();
    let auth = Authority::load_or_init(&inv).await.unwrap();

    let too_long = "a".repeat(254);
    let result = auth.sign_agent_leaf(HostId::new(), &too_long, Duration::days(30));
    let err = match result {
        Ok(_) => panic!("hostname > 253 chars should be rejected"),
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("hostname too long"),
        "got {err:#}"
    );
}
