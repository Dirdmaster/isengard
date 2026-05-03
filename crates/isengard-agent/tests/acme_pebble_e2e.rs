//! End-to-end ACME against a local Pebble (Let's Encrypt's mock CA).
//!
//! Requires Pebble running at https://localhost:14000/dir.
//! Install: `go install github.com/letsencrypt/pebble/v2/cmd/pebble@latest`
//! Or via container: `docker run -p 14000:14000 -p 15000:15000 letsencrypt/pebble`
//!
//! This test is `#[ignore]`'d so CI without Pebble passes silently.

use isengard_agent::tls::{AcmeClient, ChallengeState};
use isengard_storage::Inventory;
use std::sync::Arc;
use tempfile::tempdir;

const PEBBLE_DIR: &str = "https://localhost:14000/dir";

#[tokio::test]
#[ignore = "requires local Pebble running at localhost:14000"]
async fn order_cert_against_pebble_account_creation() {
    // Skip silently if Pebble isn't reachable (Pebble uses a self-signed cert
    // so we accept invalid TLS).
    let probe = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(PEBBLE_DIR)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await;

    if probe.is_err() {
        eprintln!("[acme_pebble_e2e] Pebble not reachable at {PEBBLE_DIR} — skipping");
        return;
    }

    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let challenges = Arc::new(ChallengeState::new());

    // NB: Pebble validates by hitting our HTTP-01 endpoint
    // (defaults to https://localhost:5002, configurable). Full end-to-end
    // requires a Pingora :8080 listener reachable from Pebble — too much
    // fixture for this scaffold. This test only exercises the
    // AcmeClient::new constructor + verifies imports compile.
    //
    // Account creation (which would actually hit Pebble) needs the
    // HTTP-01 server up to complete a real order — manual smoke.

    let _client = AcmeClient::new(
        inv,
        challenges,
        "ops@example.com".into(),
        PEBBLE_DIR.to_string(),
    );

    // Sanity: constructor + imports compile, no panic.
}
