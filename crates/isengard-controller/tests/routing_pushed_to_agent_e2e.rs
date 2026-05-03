//! Task 14 e2e: a sender registered with `RoutingPusher` receives a
//! `ProxyConfig` carrying the host's current rules when `push_to_host` runs.
//!
//! Uses the direct-pusher path (no gRPC scaffolding): the Sync handler in
//! `service.rs` only adds `register_sender` + initial-push + `unregister_sender`
//! around the same `mpsc::Sender` the test wires here. The integration test in
//! `server_skeleton.rs` already exercises the gRPC path; this test focuses
//! squarely on the registry contract introduced by Task 14.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use isengard_controller::routing::RoutingPusher;
use isengard_proto::pb::{ControllerMessage, controller_message::Payload};
use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::Status;

#[tokio::test]
async fn rule_inserted_then_push_delivers_proxy_config_to_registered_sender() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );

    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-task14".into(),
            hostname: "agent-task14".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0-test".into(),
            docker_version: "27.4.0".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let pusher = Arc::new(RoutingPusher::new(inv.clone()));

    // Register a sender as the Sync RPC handler would after receiving SyncHello.
    let (tx, mut rx) = mpsc::channel::<Result<ControllerMessage, Status>>(8);
    pusher.register_sender(host_id, tx).await;

    // Insert a routing rule for this host.
    inv.insert_routing_rule(InsertRoutingRule {
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
        state: RoutingRuleState::Active,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    })
    .await
    .unwrap();

    // Trigger the push the way a rule-change watcher (Task 16) eventually will.
    pusher.push_to_host(host_id).await.unwrap();

    let msg = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for ProxyConfig push")
        .expect("sender dropped without sending")
        .expect("push delivered an Err Status");

    let cfg = match msg.payload {
        Some(Payload::ProxyConfig(cfg)) => cfg,
        other => panic!("expected ProxyConfig payload, got {other:?}"),
    };

    assert_eq!(cfg.host_id, host_id.to_string());
    assert_eq!(cfg.generation, 1);
    assert_eq!(cfg.rules.len(), 1);
    assert_eq!(cfg.rules[0].public_hostname, "blog.example.com");
}

#[tokio::test]
async fn unregister_sender_silences_subsequent_pushes() {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );

    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-task14-unreg".into(),
            hostname: "agent-unreg".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0-test".into(),
            docker_version: "27.4.0".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();

    let pusher = Arc::new(RoutingPusher::new(inv.clone()));
    let (tx, mut rx) = mpsc::channel::<Result<ControllerMessage, Status>>(8);
    pusher.register_sender(host_id, tx).await;
    pusher.unregister_sender(host_id).await;

    // Insert a rule and push: with no registered sender, nothing should arrive.
    inv.insert_routing_rule(InsertRoutingRule {
        fleet: "default".into(),
        host_id,
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
    pusher.push_to_host(host_id).await.unwrap();

    // After unregister, the registry dropped its sender clone; with no other
    // clones outside the test, `rx.recv()` resolves to `None` (channel closed)
    // rather than timing out. Either outcome — closed-without-message OR
    // timeout — satisfies the contract: no ProxyConfig was delivered. What we
    // must NOT see is a `Some(Ok(_))` with a payload.
    match timeout(Duration::from_millis(200), rx.recv()).await {
        Err(_) => {}   // timeout: channel still open but idle — fine
        Ok(None) => {} // channel closed before any message — fine
        Ok(Some(unexpected)) => {
            panic!("no message should arrive after unregister_sender, got {unexpected:?}")
        }
    }
}
