//! Unit tests for `tls::storage::TlsStorage`.
//!
//! Covers the round-trip read/write contract, the missing-host error path, and
//! (Unix-only) the 0600 mode set on the private key file.

use isengard_agent::tls::TlsStorage;
use tempfile::TempDir;

const SAMPLE_CERT: &str =
    "-----BEGIN CERTIFICATE-----\nMIIB-test-cert\n-----END CERTIFICATE-----\n";
const SAMPLE_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIE-test-key\n-----END PRIVATE KEY-----\n";

#[tokio::test]
async fn write_then_read_returns_cert_pair() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    storage
        .write("example.com", SAMPLE_CERT, SAMPLE_KEY)
        .await
        .expect("write should succeed");

    let files = storage
        .read("example.com")
        .await
        .expect("read should succeed");

    assert_eq!(files.cert_pem, SAMPLE_CERT);
    assert_eq!(files.key_pem, SAMPLE_KEY);
}

#[tokio::test]
async fn read_missing_returns_err() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    let err = storage
        .read("nope.example.com")
        .await
        .expect_err("missing host should error");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("nope.example.com"),
        "error should mention hostname, got: {msg}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn write_sets_mode_0600_on_key() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    storage
        .write("secure.example.com", SAMPLE_CERT, SAMPLE_KEY)
        .await
        .expect("write should succeed");

    let key_path = tmp.path().join("secure.example.com.key");
    let meta = std::fs::metadata(&key_path).expect("key file should exist");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "key file mode should be 0600, got {mode:o}");
}
