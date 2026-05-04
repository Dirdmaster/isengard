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

#[tokio::test]
async fn write_rejects_path_traversal_hostname() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    for bad in ["../escape", "foo/bar", "foo\\bar", "evil..host", ""] {
        storage
            .write(bad, SAMPLE_CERT, SAMPLE_KEY)
            .await
            .expect_err(&format!("should reject hostname {bad:?}"));
    }
    // Verify nothing was written to disk for any rejected attempt.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.file_name())).collect())
        .unwrap_or_default();
    assert!(
        entries.is_empty(),
        "no files should have been written, got {entries:?}"
    );
}

#[tokio::test]
async fn write_rejects_invalid_hostname_chars() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    for bad in ["foo bar", "foo:443", "foo*", "foo;rm"] {
        storage
            .write(bad, SAMPLE_CERT, SAMPLE_KEY)
            .await
            .expect_err(&format!("should reject hostname {bad:?}"));
    }
}

#[tokio::test]
async fn write_rejects_malformed_cert_pem() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    storage
        .write("ok.example.com", "not a cert", SAMPLE_KEY)
        .await
        .expect_err("should reject cert without BEGIN marker");
}

#[tokio::test]
async fn write_rejects_malformed_key_pem() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    storage
        .write("ok.example.com", SAMPLE_CERT, "not a key")
        .await
        .expect_err("should reject key without BEGIN/PRIVATE KEY markers");
}

#[tokio::test]
async fn write_accepts_rsa_and_ec_private_key_variants() {
    let tmp = TempDir::new().unwrap();
    let storage = TlsStorage::new(tmp.path().to_path_buf());

    let rsa_key = "-----BEGIN RSA PRIVATE KEY-----\nfake\n-----END RSA PRIVATE KEY-----\n";
    let ec_key = "-----BEGIN EC PRIVATE KEY-----\nfake\n-----END EC PRIVATE KEY-----\n";

    storage
        .write("rsa.example.com", SAMPLE_CERT, rsa_key)
        .await
        .expect("RSA private key should be accepted");
    storage
        .write("ec.example.com", SAMPLE_CERT, ec_key)
        .await
        .expect("EC private key should be accepted");
}
