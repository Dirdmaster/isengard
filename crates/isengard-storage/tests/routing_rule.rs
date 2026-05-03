use isengard_storage::{
    EnrollHost, HostId, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use tempfile::tempdir;

async fn seed_host(inv: &Inventory) -> HostId {
    inv.enroll_host(EnrollHost {
        fingerprint: "fp-1".into(),
        hostname: "h1".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.0.1".into(),
        docker_version: "27.0".into(),
        fleet: "default".into(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn insert_then_list_by_host_returns_inserted() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host_id = seed_host(&inv).await;

    let inserted = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 8080,
            public_hostname: "blog.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: Some("/healthz".into()),
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    let listed = inv.list_routing_rules_for_host(host_id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, inserted.id);
    assert_eq!(listed[0].public_hostname, "blog.example.com");
}

#[tokio::test]
async fn insert_unique_violation_returns_err() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host_id = seed_host(&inv).await;

    let make = |hostname: &str| InsertRoutingRule {
        fleet: "default".into(),
        host_id,
        stack_id: None,
        service_name: "web".into(),
        container_port: 80,
        public_hostname: hostname.into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Acme,
        healthcheck_path: None,
        healthcheck_interval_secs: 10,
        auth: None,
        state: RoutingRuleState::Pending,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    };

    inv.insert_routing_rule(make("dup.example.com"))
        .await
        .unwrap();
    let err = inv
        .insert_routing_rule(make("dup.example.com"))
        .await
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("unique"));
}

#[tokio::test]
async fn delete_routing_rule_removes_row() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host_id = seed_host(&inv).await;

    let r = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: None,
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "x.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("manual"))
        .await
        .unwrap();
    inv.delete_routing_rule(r.id).await.unwrap();

    let still = inv.list_routing_rules_for_host(host_id).await.unwrap();
    assert!(still.is_empty(), "rule should be gone");

    let overrides = inv.list_routing_rule_overrides(r.id).await.unwrap();
    assert!(overrides.is_empty(), "overrides should cascade-delete");
}

#[tokio::test]
async fn upsert_override_then_list_returns_value() {
    let dir = tempdir().unwrap();
    let inv = Inventory::open(&dir.path().join("isengard.db"))
        .await
        .unwrap();
    let host = seed_host(&inv).await;

    let r = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host,
            stack_id: None,
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "ov.example.com".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Pending,
            source: RoutingRuleSource::Label,
            source_container_id: Some("cid-1".into()),
            source_imported_from: None,
        })
        .await
        .unwrap();

    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("manual"))
        .await
        .unwrap();
    inv.upsert_routing_rule_override(r.id, "tls_mode", serde_json::json!("edge"))
        .await
        .unwrap();

    let list = inv.list_routing_rule_overrides(r.id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].field, "tls_mode");
    assert_eq!(list[0].value_json, serde_json::json!("edge"));
}
