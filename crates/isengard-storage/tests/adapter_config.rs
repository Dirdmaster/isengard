use isengard_storage::{AdapterConfig, EnrollHost, Inventory, UpsertAdapterConfig};
use tempfile::tempdir;

#[tokio::test]
async fn upsert_then_get_returns_config() {
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

    inv.upsert_adapter_config(UpsertAdapterConfig {
        host_id,
        adapter: "none".into(),
        config_json: serde_json::json!({}),
        enabled: true,
    })
    .await
    .unwrap();

    let cfg: AdapterConfig = inv
        .get_adapter_config(host_id, "none")
        .await
        .unwrap()
        .expect("config exists");
    assert_eq!(cfg.adapter, "none");
    assert!(cfg.enabled);

    // upsert overwrites
    inv.upsert_adapter_config(UpsertAdapterConfig {
        host_id,
        adapter: "none".into(),
        config_json: serde_json::json!({"k": "v"}),
        enabled: false,
    })
    .await
    .unwrap();

    let cfg = inv
        .get_adapter_config(host_id, "none")
        .await
        .unwrap()
        .unwrap();
    assert!(!cfg.enabled);
    assert_eq!(cfg.config_json, serde_json::json!({"k": "v"}));
}
