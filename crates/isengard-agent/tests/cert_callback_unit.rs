use isengard_agent::tls::cert_store::CertStore;
use isengard_agent::tls::storage::TlsStorage;
use rcgen::{CertificateParams, KeyPair};
use std::sync::Arc;
use tempfile::tempdir;

fn issue_self_signed(hostname: &str) -> (String, String) {
    let key_pair = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![hostname.to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, hostname.to_string());
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair.serialize_pem())
}

#[tokio::test]
async fn lookup_loads_from_filesystem_on_cache_miss() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let (cert_pem, key_pem) = issue_self_signed("h.test");
    storage.write("h.test", &cert_pem, &key_pem).await.unwrap();

    let store = Arc::new(CertStore::new(storage.clone()));
    let entry = store.lookup("h.test").await.expect("loads from disk");
    assert!(!entry.leaf.to_pem().unwrap().is_empty());
}

#[tokio::test]
async fn lookup_returns_none_when_neither_cache_nor_disk_has_it() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let store = Arc::new(CertStore::new(storage));
    assert!(store.lookup("does-not-exist.test").await.is_none());
}

#[tokio::test]
async fn install_then_lookup_serves_from_cache() {
    let dir = tempdir().unwrap();
    let storage = TlsStorage::new(dir.path().to_path_buf());
    let store = Arc::new(CertStore::new(storage));

    let (cert_pem, key_pem) = issue_self_signed("h.test");
    store.install("h.test", &cert_pem, &key_pem).await.unwrap();
    let entry = store.lookup("h.test").await.expect("served from cache");
    assert!(!entry.leaf.to_pem().unwrap().is_empty());
}
