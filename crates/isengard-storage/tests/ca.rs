use isengard_storage::Inventory;
use isengard_storage::ca::CaRow;

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

#[tokio::test]
async fn get_ca_returns_none_when_unset() {
    let inv = fresh_inv().await;
    assert!(inv.get_ca().await.unwrap().is_none());
}

#[tokio::test]
async fn set_ca_then_get_round_trips() {
    let inv = fresh_inv().await;
    let row = CaRow {
        root_cert_pem: "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n".into(),
        root_key_pem: "-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----\n".into(),
    };
    inv.set_ca(row.clone()).await.unwrap();
    let got = inv.get_ca().await.unwrap().expect("ca present");
    assert_eq!(got.root_cert_pem, row.root_cert_pem);
    assert_eq!(got.root_key_pem, row.root_key_pem);
}

#[tokio::test]
async fn set_ca_twice_errors() {
    let inv = fresh_inv().await;
    let row = CaRow {
        root_cert_pem: "cert".into(),
        root_key_pem: "key".into(),
    };
    inv.set_ca(row.clone()).await.unwrap();
    let err = inv.set_ca(row).await.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("ca"));
}
