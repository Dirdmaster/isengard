use isengard_agent::cert_store::{CertBundle, exists, load, save};

fn fixture() -> CertBundle {
    CertBundle {
        ca_pem: "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n".into(),
        cert_pem: "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n".into(),
        key_pem: "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n".into(),
    }
}

#[test]
fn exists_false_on_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!exists(tmp.path()));
}

#[test]
fn save_then_load_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle = fixture();
    save(tmp.path(), &bundle).unwrap();
    assert!(exists(tmp.path()));
    let loaded = load(tmp.path()).unwrap();
    assert_eq!(loaded.ca_pem, bundle.ca_pem);
    assert_eq!(loaded.cert_pem, bundle.cert_pem);
    assert_eq!(loaded.key_pem, bundle.key_pem);
}

#[test]
fn save_writes_key_with_restrictive_perms() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    save(tmp.path(), &fixture()).unwrap();
    let key_meta = std::fs::metadata(tmp.path().join("certs").join("agent.key")).unwrap();
    let mode = key_meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "key file should be chmod 600, got {:o}", mode);
}

#[test]
fn save_cleans_up_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    save(tmp.path(), &fixture()).unwrap();
    let new_bundle = CertBundle {
        ca_pem: "new-ca".into(),
        cert_pem: "new-cert".into(),
        key_pem: "new-key".into(),
    };
    save(tmp.path(), &new_bundle).unwrap();

    let cert_dir = tmp.path().join("certs");
    assert!(
        !cert_dir.join("ca.pem.new").exists(),
        ".new files must be cleaned up"
    );
    assert!(!cert_dir.join("agent.crt.new").exists());
    assert!(!cert_dir.join("agent.key.new").exists());
}
