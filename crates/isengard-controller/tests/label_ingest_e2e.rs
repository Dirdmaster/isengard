//! End-to-end (no gRPC): drive `RoutingPusher::ingest_labels` and
//! `ingest_labels_removed` directly and verify the storage side ends up in
//! the expected shape.

use isengard_proto::pb::{ContainerLabelsRemoved, ContainerLabelsReport};
use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::{TempDir, tempdir};

async fn setup() -> (
    TempDir,
    Arc<Inventory>,
    isengard_storage::HostId,
    Arc<isengard_controller::routing::RoutingPusher>,
) {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
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
    let pusher = Arc::new(isengard_controller::routing::RoutingPusher::new(
        inv.clone(),
    ));
    (dir, inv, host, pusher)
}

#[tokio::test]
async fn labels_report_creates_routing_rule_with_label_source() {
    let (_dir, inv, host, pusher) = setup().await;

    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "lbl.example.com".to_string());
    labels.insert("isengard.expose.port".to_string(), "8080".to_string());

    pusher
        .ingest_labels(
            host,
            ContainerLabelsReport {
                container_id: "cid-1".into(),
                container_name: "web".into(),
                image: "nginx:1.25".into(),
                labels,
            },
        )
        .await
        .unwrap();

    let rules = inv.list_routing_rules_for_host(host).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].public_hostname, "lbl.example.com");
    assert_eq!(rules[0].container_port, 8080);
    assert_eq!(rules[0].source, RoutingRuleSource::Label);
    assert_eq!(rules[0].source_container_id.as_deref(), Some("cid-1"));
}

#[tokio::test]
async fn labels_removed_event_deletes_label_rules_for_container() {
    let (_dir, inv, host, pusher) = setup().await;

    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "x.test".to_string());
    pusher
        .ingest_labels(
            host,
            ContainerLabelsReport {
                container_id: "cid-A".into(),
                container_name: "web".into(),
                image: "n:1".into(),
                labels,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        inv.list_routing_rules_for_host(host).await.unwrap().len(),
        1
    );

    pusher
        .ingest_labels_removed(
            host,
            ContainerLabelsRemoved {
                container_id: "cid-A".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        inv.list_routing_rules_for_host(host)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn label_arriving_for_existing_ui_hostname_replaces_ui_rule() {
    let (_dir, inv, host, pusher) = setup().await;

    // Pre-existing UI rule on hostname "blog.test"
    inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id: host,
        stack_id: None,
        service_name: "old".into(),
        container_port: 80,
        public_hostname: "blog.test".into(),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Manual,
        healthcheck_path: None,
        healthcheck_interval_secs: 10,
        auth: None,
        state: RoutingRuleState::Active,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    })
    .await
    .unwrap();

    // Label-discovered rule arrives for the same hostname.
    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "blog.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "8080".to_string());

    pusher
        .ingest_labels(
            host,
            ContainerLabelsReport {
                container_id: "cid".into(),
                container_name: "newweb".into(),
                image: "n:1".into(),
                labels,
            },
        )
        .await
        .unwrap();

    let rules = inv.list_routing_rules_for_host(host).await.unwrap();
    let by_host: Vec<_> = rules
        .iter()
        .filter(|r| r.public_hostname == "blog.test")
        .collect();
    assert_eq!(by_host.len(), 1, "should be exactly one rule for blog.test");
    assert_eq!(by_host[0].source, RoutingRuleSource::Label);
    assert_eq!(by_host[0].service_name, "newweb");
}

#[tokio::test]
async fn label_displacing_ui_rule_preserves_overrides() {
    let (_dir, inv, host, pusher) = setup().await;

    let ui = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id: host,
            stack_id: None,
            service_name: "old".into(),
            container_port: 80,
            public_hostname: "ov.test".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Acme,
            healthcheck_path: None,
            healthcheck_interval_secs: 10,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    inv.upsert_routing_rule_override(ui.id, "tls_mode", serde_json::json!("manual"))
        .await
        .unwrap();

    let mut labels = HashMap::new();
    labels.insert("isengard.expose".to_string(), "ov.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "8080".to_string());

    pusher
        .ingest_labels(
            host,
            ContainerLabelsReport {
                container_id: "cid".into(),
                container_name: "web".into(),
                image: "n".into(),
                labels,
            },
        )
        .await
        .unwrap();

    let rules = inv.list_routing_rules_for_host(host).await.unwrap();
    let new_rule = rules
        .iter()
        .find(|r| r.public_hostname == "ov.test")
        .unwrap();
    assert_eq!(new_rule.source, RoutingRuleSource::Label);

    let overs = inv.list_routing_rule_overrides(new_rule.id).await.unwrap();
    assert_eq!(overs.len(), 1);
    assert_eq!(overs[0].field, "tls_mode");
    assert_eq!(overs[0].value_json, serde_json::json!("manual"));
}
