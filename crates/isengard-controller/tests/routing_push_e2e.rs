//! Push-side test: insert a routing rule for a host, assert RoutingPusher
//! produces a ProxyConfig message containing it, with monotonic generation.

use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn build_proxy_config_for_host_includes_rule() {
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
        })
        .await
        .unwrap();

    let _r = inv
        .insert_routing_rule(InsertRoutingRule {
            host_id: host,
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
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    let pusher = isengard_controller::routing::RoutingPusher::new(inv.clone());
    let cfg = pusher.build_for_host(host).await.unwrap();
    assert_eq!(cfg.rules.len(), 1);
    assert_eq!(cfg.rules[0].public_hostname, "blog.example.com");
    assert_eq!(cfg.generation, 1);

    // Calling again does NOT increment without a change.
    let cfg2 = pusher.build_for_host(host).await.unwrap();
    assert_eq!(cfg2.generation, 1);

    // Adding another rule increments.
    inv.insert_routing_rule(InsertRoutingRule {
        host_id: host,
        stack_id: None,
        service_name: "api".into(),
        container_port: 8081,
        public_hostname: "api.example.com".into(),
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

    let cfg3 = pusher.build_for_host(host).await.unwrap();
    assert_eq!(cfg3.generation, 2);
    assert_eq!(cfg3.rules.len(), 2);
}
