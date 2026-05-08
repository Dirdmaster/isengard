use isengard_storage::{Inventory, secret::validate_secret_name};

async fn fresh_inv() -> Inventory {
    Inventory::open_in_memory().await.expect("open")
}

#[tokio::test]
async fn upsert_then_list_round_trips_meta_only() {
    let inv = fresh_inv().await;
    inv.upsert_secret("foo", &[0x01, 0x02, 0x03], Some("operator"))
        .await
        .unwrap();
    inv.upsert_secret("bar", &[0xff], None).await.unwrap();
    let metas = inv.list_secrets().await.unwrap();
    assert_eq!(metas.len(), 2);
    let names: Vec<_> = metas.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["bar", "foo"]); // alpha-sorted
    let foo = metas.iter().find(|m| m.name == "foo").unwrap();
    assert_eq!(foo.created_by.as_deref(), Some("operator"));
}

#[tokio::test]
async fn get_ciphertext_returns_exact_bytes() {
    let inv = fresh_inv().await;
    let bytes = vec![0x10, 0x20, 0x30, 0x40, 0x50];
    inv.upsert_secret("token", &bytes, None).await.unwrap();
    let got = inv.get_secret_ciphertext("token").await.unwrap().unwrap();
    assert_eq!(got, bytes);
    assert!(
        inv.get_secret_ciphertext("missing")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn upsert_replaces_value_and_updates_timestamp() {
    let inv = fresh_inv().await;
    inv.upsert_secret("k", &[1, 1, 1], None).await.unwrap();
    let first = inv.get_secret_meta("k").await.unwrap().unwrap();
    // Sleep one second so the rfc3339 timestamps differ.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    inv.upsert_secret("k", &[2, 2, 2], None).await.unwrap();
    let second = inv.get_secret_meta("k").await.unwrap().unwrap();
    assert!(second.updated_at > first.updated_at);
    let cipher = inv.get_secret_ciphertext("k").await.unwrap().unwrap();
    assert_eq!(cipher, vec![2, 2, 2]);
}

#[tokio::test]
async fn insert_strict_rejects_duplicate() {
    let inv = fresh_inv().await;
    inv.insert_secret_strict("dup", &[1], None).await.unwrap();
    let err = inv
        .insert_secret_strict("dup", &[2], None)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("already exists"));
}

#[tokio::test]
async fn delete_returns_whether_removed() {
    let inv = fresh_inv().await;
    inv.upsert_secret("d", &[1], None).await.unwrap();
    assert!(inv.delete_secret("d").await.unwrap());
    assert!(!inv.delete_secret("d").await.unwrap());
    assert!(inv.get_secret_ciphertext("d").await.unwrap().is_none());
}

#[tokio::test]
async fn has_any_secret_predicate() {
    let inv = fresh_inv().await;
    assert!(!inv.has_any_secret().await.unwrap());
    inv.upsert_secret("present", &[1], None).await.unwrap();
    assert!(inv.has_any_secret().await.unwrap());
}

#[test]
fn validate_secret_name_accepts_safe_chars() {
    for ok in ["a", "FOO", "foo.bar", "foo_bar-1", "x".repeat(64).as_str()] {
        assert!(validate_secret_name(ok).is_ok(), "{ok} should pass");
    }
}

#[test]
fn validate_secret_name_rejects_bad_inputs() {
    for bad in [
        "",
        "has space",
        "with/slash",
        "co:lon",
        "x".repeat(65).as_str(),
    ] {
        assert!(validate_secret_name(bad).is_err(), "{bad} should fail");
    }
}

#[tokio::test]
async fn list_secrets_never_exposes_ciphertext_field() {
    // Compile-time check: SecretMeta has no ciphertext-shaped field. We
    // confirm by serializing one and asserting the JSON shape.
    let inv = fresh_inv().await;
    inv.upsert_secret("api_key", &[0xde, 0xad, 0xbe, 0xef], None)
        .await
        .unwrap();
    let metas = inv.list_secrets().await.unwrap();
    let json = serde_json::to_string(&metas).unwrap();
    assert!(!json.contains("ciphertext"));
    assert!(!json.contains("dead"));
    assert!(!json.contains("0xde"));
}
