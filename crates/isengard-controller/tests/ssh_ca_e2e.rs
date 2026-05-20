//! End-to-end tests for the SSH user-certificate authority.
//!
//! Covers the two contractual behaviours of [`SshAuthority`]:
//!
//! 1. Persistence: a fresh mint round-trips through the encrypted secrets
//!    store. A second load against the same store returns an authority
//!    whose public key matches the first.
//! 2. Cert issuance: a signed user cert parses back via the standard
//!    `ssh-key` decoder with the expected cert type, principals, key id,
//!    and validity window.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use isengard_controller::secrets::SecretsStore;
use isengard_controller::ssh_ca::SshAuthority;
use isengard_storage::Inventory;
use ssh_key::certificate::CertType;
use ssh_key::{Algorithm, Certificate, PrivateKey, PublicKey};

/// Build an in-memory secrets store with a deterministic master key.
///
/// The key value does not matter for these tests; what matters is that
/// the same store is reused across two `load_or_init` calls so the second
/// finds the persisted row.
async fn store_with_fixed_key() -> SecretsStore {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    SecretsStore::new(inv, key)
}

#[tokio::test]
async fn load_or_init_persists_the_keypair() {
    let store = store_with_fixed_key().await;

    let first = SshAuthority::load_or_init(&store)
        .await
        .expect("first load mints a fresh ssh ca");
    let first_pub = first.public_key_openssh().to_vec();

    let second = SshAuthority::load_or_init(&store)
        .await
        .expect("second load reuses the persisted keypair");
    let second_pub = second.public_key_openssh().to_vec();

    assert_eq!(
        first_pub, second_pub,
        "ssh ca public key changed across reload: not persisted",
    );
    assert!(!first_pub.is_empty(), "ssh ca public key bytes are empty",);
}

#[tokio::test]
async fn sign_user_cert_produces_a_verifiable_openssh_cert() {
    let store = store_with_fixed_key().await;
    let ca = SshAuthority::load_or_init(&store)
        .await
        .expect("load ssh ca");

    // Mint a target operator pubkey via the `ssh-key` crate. In real use
    // this comes from the operator's `~/.ssh/id_ed25519.pub`; for the test
    // we generate a throwaway pair.
    let operator_priv = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
        .expect("mint operator keypair");
    let operator_pub: PublicKey = operator_priv.public_key().clone();

    let ttl = Duration::from_secs(3600);
    let principals = vec!["dirdmaster".to_string()];
    let before_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let cert_bytes = ca
        .sign_user_cert(&operator_pub, &principals, ttl, "macbook-air")
        .expect("sign user cert");

    let after_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let openssh = std::str::from_utf8(&cert_bytes).expect("cert is utf-8");
    let cert = Certificate::from_openssh(openssh).expect("cert parses as openssh");

    assert_eq!(cert.cert_type(), CertType::User, "cert type");
    assert!(
        cert.valid_principals().iter().any(|p| p == "dirdmaster"),
        "principals missing dirdmaster: {:?}",
        cert.valid_principals(),
    );
    assert_eq!(cert.key_id(), "macbook-air", "key_id");

    // valid_after / valid_before bracket "now" by approximately ttl. The
    // signing path captures `SystemTime::now()` once; `before_unix` and
    // `after_unix` bracket that capture, so the assertion checks the
    // window in seconds.
    let valid_after = cert.valid_after();
    let valid_before = cert.valid_before();
    assert!(
        valid_after >= before_unix && valid_after <= after_unix,
        "valid_after {valid_after} not in [{before_unix}, {after_unix}]",
    );
    assert!(
        valid_before >= before_unix + ttl.as_secs() && valid_before <= after_unix + ttl.as_secs(),
        "valid_before {valid_before} not in [{}, {}]",
        before_unix + ttl.as_secs(),
        after_unix + ttl.as_secs(),
    );
}
