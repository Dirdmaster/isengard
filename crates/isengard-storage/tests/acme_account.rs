use isengard_storage::{AcmeAccount, Inventory, UpsertAcmeAccount};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_account() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "ops@example.com".into(),
        directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "-----BEGIN PRIVATE KEY-----\nMIGkAg...\n-----END PRIVATE KEY-----\n"
            .into(),
        kid: Some("https://acme-v02.api.letsencrypt.org/acme/acct/12345".into()),
    })
    .await
    .unwrap();

    let acct: AcmeAccount = inv.get_acme_account().await.unwrap().expect("exists");
    assert_eq!(acct.contact_email, "ops@example.com");
    assert_eq!(
        acct.directory_url,
        "https://acme-v02.api.letsencrypt.org/directory"
    );
    assert!(
        acct.account_key_pem
            .starts_with("-----BEGIN PRIVATE KEY-----")
    );
    assert_eq!(
        acct.kid.as_deref(),
        Some("https://acme-v02.api.letsencrypt.org/acme/acct/12345")
    );
}

#[tokio::test]
async fn upsert_overwrites_existing_singleton() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "first@example.com".into(),
        directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "key-v1".into(),
        kid: None,
    })
    .await
    .unwrap();

    inv.upsert_acme_account(UpsertAcmeAccount {
        contact_email: "second@example.com".into(),
        directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
        account_key_pem: "key-v2".into(),
        kid: Some("kid-v2".into()),
    })
    .await
    .unwrap();

    let acct: AcmeAccount = inv.get_acme_account().await.unwrap().expect("exists");
    assert_eq!(acct.contact_email, "second@example.com");
    assert_eq!(
        acct.directory_url,
        "https://acme-v02.api.letsencrypt.org/directory"
    );
    assert_eq!(acct.account_key_pem, "key-v2");
    assert_eq!(acct.kid.as_deref(), Some("kid-v2"));
}
