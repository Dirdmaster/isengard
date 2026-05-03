use chrono::Utc;
use isengard_storage::{EnrollHost, Inventory, TlsCertMeta, UpsertTlsCertMeta};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_meta() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let now = Utc::now();
    inv.upsert_tls_cert_meta(UpsertTlsCertMeta {
        public_hostname: "blog.example.com".into(),
        host_id,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + chrono::Duration::days(90),
        next_renewal_at: now + chrono::Duration::days(60),
        serial: Some("0xabc".into()),
    })
    .await
    .unwrap();

    let m: TlsCertMeta = inv
        .get_tls_cert_meta("blog.example.com")
        .await
        .unwrap()
        .expect("exists");
    assert_eq!(m.issuer, "lets_encrypt");
    assert_eq!(m.serial.as_deref(), Some("0xabc"));
}

#[tokio::test]
async fn record_attempt_increments_count_and_clears_error_on_success() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let now = Utc::now();
    inv.upsert_tls_cert_meta(UpsertTlsCertMeta {
        public_hostname: "blog.example.com".into(),
        host_id: host,
        issuer: "lets_encrypt".into(),
        not_before: now,
        not_after: now + chrono::Duration::days(90),
        next_renewal_at: now + chrono::Duration::days(60),
        serial: None,
    })
    .await
    .unwrap();

    inv.record_tls_attempt("blog.example.com", false, Some("rate limited".into()))
        .await
        .unwrap();
    let m = inv
        .get_tls_cert_meta("blog.example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.attempt_count, 1);
    assert_eq!(m.last_error.as_deref(), Some("rate limited"));

    inv.record_tls_attempt("blog.example.com", true, None)
        .await
        .unwrap();
    let m = inv
        .get_tls_cert_meta("blog.example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.attempt_count, 2);
    assert_eq!(m.last_error, None, "success clears error");
}
