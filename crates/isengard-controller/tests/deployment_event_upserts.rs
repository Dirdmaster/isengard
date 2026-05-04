//! Integration test: publish a `deployment.*` event with an embedded Deployment
//! into the bus; assert the parallel handler task upserts that row into the
//! controller-side inventory.
//!
//! This mirrors the wiring in `run_controller`: a `tokio::spawn` task subscribes
//! to the bus, filters by `deployment.` prefix, deserialises
//! `event.metadata.deployment`, and calls `upsert_deployment_from_remote`.
//! Re-creating the spawn loop here (rather than calling `run_controller`) keeps
//! the test focused and free of the gRPC/auth layer.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use isengard_controller::bus::EventBus;
use isengard_core::Event;
use isengard_storage::deployment::{DeployStrategy, Deployment, DeploymentState};
use isengard_storage::stack::StackSource;
use isengard_storage::{EnrollHost, InsertStack, Inventory};
use tempfile::tempdir;

#[tokio::test]
async fn deployment_event_upserts_into_inventory() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("isengard.db");

    let inventory = Arc::new(Inventory::open(&db).await.unwrap());
    let bus = Arc::new(EventBus::new());

    // A host + stack for the deployment row to reference.
    let host_id = inventory
        .enroll_host(EnrollHost {
            fingerprint: "fp-bus".to_string(),
            hostname: "h-bus".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            agent_version: "0.1.0-test".to_string(),
            docker_version: "27.4.0".to_string(),
            fleet: "test".to_string(),
        })
        .await
        .unwrap();
    let stack_id = inventory
        .insert_stack(InsertStack {
            host_id,
            name: "blog".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    // Spawn the same handler shape that `run_controller` wires up, against
    // this test's bus + inventory.
    let handler_inv = inventory.clone();
    let mut rx = bus.subscribe();
    let handle = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event.kind.starts_with("deployment.") {
                        continue;
                    }
                    let Some(dep_value) = event.metadata.get("deployment") else {
                        continue;
                    };
                    let dep: Deployment = match serde_json::from_value(dep_value.clone()) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    let _ = handler_inv.upsert_deployment_from_remote(&dep).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Build a Deployment as if it arrived from a remote agent.
    let dep_id = ulid::Ulid::new().to_string();
    let now = Utc::now();
    let dep = Deployment {
        id: dep_id.clone(),
        host_id,
        stack_id,
        service_name: "web".into(),
        strategy: DeployStrategy::BlueGreen,
        state: DeploymentState::SpinningUp,
        blue_container: Some("c-blue".into()),
        green_container: None,
        blue_digest: "sha256:aaa".into(),
        green_digest: "sha256:bbb".into(),
        public_hostname: Some("blog.test".into()),
        health_path: Some("/healthz".into()),
        container_port: Some(8080),
        healthcheck_started_at: None,
        healthcheck_passed_at: None,
        switched_at: None,
        drained_at: None,
        finished_at: None,
        error: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };

    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "deployment".to_string(),
        serde_json::to_value(&dep).unwrap(),
    );

    bus.publish(Event {
        kind: "deployment.spinning_up".into(),
        occurred_at: now,
        host_id: Some(host_id.into()),
        summary: "spinning up".into(),
        metadata: serde_json::Value::Object(metadata),
        ..Default::default()
    });

    // Poll the inventory for the upserted row. The handler is async; give it a
    // generous window before declaring failure.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got: Option<Deployment> = None;
    while std::time::Instant::now() < deadline {
        if let Some(d) = inventory.get_deployment(&dep_id).await.unwrap() {
            got = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    handle.abort();

    let row = got.expect("deployment row should have been upserted by the handler");
    assert_eq!(row.id, dep_id);
    assert_eq!(row.state, DeploymentState::SpinningUp);
    assert_eq!(row.service_name, "web");
    assert_eq!(row.blue_container.as_deref(), Some("c-blue"));
}
