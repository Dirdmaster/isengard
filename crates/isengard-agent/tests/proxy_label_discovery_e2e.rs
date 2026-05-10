//! Real Docker e2e for the label-discovery → routing-rule pipeline.
//!
//! Requires a running `dockerd` reachable via the local socket. When Docker
//! isn't available, the test silently returns (so CI without Docker doesn't
//! flake). Marked `#[ignore]` so `cargo test` skips it by default; opt in via:
//!
//!     cargo test -p isengard-agent --test proxy_label_discovery_e2e -- --ignored
//!
//! Flow:
//!   1. ping Docker (skip if unreachable)
//!   2. spawn `labels::watch` with an mpsc channel
//!   3. pull busybox + start a container with `isengard.expose=e2e.test` labels
//!   4. wait for the matching `ContainerLabelsReport` on the channel
//!   5. drive `RoutingPusher::ingest_labels` directly, then assert storage
//!      ends up with one routing rule for `e2e.test`
//!   6. tear the container down

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, KillContainerOptions, RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use isengard_proto::pb::{AgentMessage, agent_message::Payload};
use isengard_storage::{EnrollHost, Inventory};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "requires running dockerd; opt in via --ignored"]
async fn label_on_real_container_creates_routing_rule() {
    // 1. Skip silently if no Docker socket / daemon.
    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(_) => return,
    };
    if docker.ping().await.is_err() {
        return;
    }

    // 2. Storage + RoutingPusher (no full controller startup needed).
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );
    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-e2e".into(),
            hostname: "h-e2e".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();
    let pusher = isengard_controller::routing::RoutingPusher::new(inv.clone());

    // 3. Spawn the labels watcher; it forwards into our channel. Phase 0.5
    //    moved the watcher onto `RuntimeBackend`; build a BollardBackend
    //    against a tempdir state_dir to drive it.
    let (tx, mut rx) = mpsc::channel::<AgentMessage>(16);
    let backend_state_dir = tempdir().unwrap();
    let backend: Arc<dyn isengard_agent::runtime::RuntimeBackend> = Arc::new(
        isengard_agent::runtime::bollard_backend::BollardBackend::from_env(
            backend_state_dir.path(),
        )
        .await
        .expect("BollardBackend::from_env"),
    );
    let watcher = tokio::spawn(async move {
        let _ = isengard_agent::labels::watch(backend, tx).await;
    });

    // 4. Pull busybox if needed (often local already; ignore stream errors).
    let mut pull = docker.create_image(
        Some(CreateImageOptions::<&str> {
            from_image: "busybox:latest",
            ..Default::default()
        }),
        None,
        None,
    );
    while pull.next().await.is_some() {}

    // 5. Create + start a container with the isengard.expose labels.
    let mut labels: HashMap<String, String> = HashMap::new();
    labels.insert("isengard.expose".to_string(), "e2e.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "80".to_string());

    let container_name = format!("isengard-e2e-{}", std::process::id());
    let cont = docker
        .create_container(
            Some(CreateContainerOptions {
                name: container_name.clone(),
                platform: None,
            }),
            Config {
                image: Some("busybox:latest".to_string()),
                cmd: Some(vec!["sleep".to_string(), "30".to_string()]),
                labels: Some(labels.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("create container");
    docker
        .start_container::<String>(&cont.id, None)
        .await
        .expect("start container");

    // 6. Drain reports until we see the one we just created. The watcher's
    //    initial scan may surface other isengard-labelled containers first;
    //    we filter by hostname to be robust.
    let report = loop {
        let msg = timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("timed out waiting for label report")
            .expect("watcher channel closed");
        if let Some(Payload::ContainerLabelsReport(r)) = msg.payload {
            if r.labels.get("isengard.expose").map(String::as_str) == Some("e2e.test") {
                break r;
            }
        }
    };

    pusher
        .ingest_labels(host, report)
        .await
        .expect("ingest_labels");

    // 7. Storage should now contain exactly one rule for e2e.test.
    let rules = inv.list_routing_rules_for_host(host).await.unwrap();
    let by_host: Vec<_> = rules
        .iter()
        .filter(|r| r.public_hostname == "e2e.test")
        .collect();
    assert_eq!(by_host.len(), 1, "expected one rule for e2e.test");
    assert_eq!(by_host[0].container_port, 80);

    // 8. Cleanup. Best-effort; ignore errors so a partial failure still cleans
    //    up as much as possible.
    let _ = docker
        .kill_container(&cont.id, None::<KillContainerOptions<String>>)
        .await;
    let _ = docker
        .remove_container(
            &cont.id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    watcher.abort();
}
