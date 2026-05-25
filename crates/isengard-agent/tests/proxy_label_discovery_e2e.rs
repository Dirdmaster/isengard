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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, KillContainerOptions, ListContainersOptions,
    RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::network::InspectNetworkOptions;
use futures_util::StreamExt;
use isengard_agent::proxy::SHARED_PROXY_NETWORK;
use isengard_agent::runtime::bollard_backend::BollardBackend;
use isengard_agent::runtime::{IngressEndpoint, IngressEndpointMode, RuntimeBackend};
use isengard_proto::pb::{AgentMessage, agent_message::Payload};
use isengard_storage::{EnrollHost, Inventory};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::timeout;

const AUTOATTACH_TEST_LABEL_KEY: &str = "isengard.test";
const AUTOATTACH_TEST_LABEL_VALUE: &str = "autoattach";
const AUTOATTACH_TEST_LABEL: &str = "isengard.test=autoattach";
const AUTOATTACH_TEST_NAME_PREFIX: &str = "isengard-e2e-autoattach-";

async fn cleanup_autoattach_test_containers(docker: &Docker) {
    let mut filters = HashMap::new();
    filters.insert("label".to_string(), vec![AUTOATTACH_TEST_LABEL.to_string()]);

    let mut ids = HashSet::new();

    if let Ok(containers) = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            filters,
            ..Default::default()
        }))
        .await
    {
        for container in containers {
            if let Some(id) = container.id {
                ids.insert(id);
            }
        }
    }

    if let Ok(containers) = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        }))
        .await
    {
        for container in containers {
            let name_matches = container.names.as_ref().is_some_and(|names| {
                names.iter().any(|name| {
                    name.trim_start_matches('/')
                        .starts_with(AUTOATTACH_TEST_NAME_PREFIX)
                })
            });
            if name_matches {
                if let Some(id) = container.id {
                    ids.insert(id);
                }
            }
        }
    }

    for id in ids {
        let _ = docker
            .kill_container(&id, None::<KillContainerOptions<String>>)
            .await;
        let _ = docker
            .remove_container(
                &id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
    }
}

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
        })
        .await
        .unwrap();
    let pusher = isengard_controller::routing::RoutingPusher::new(inv.clone());

    // 3. Spawn the labels watcher; it forwards into our channel.
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

    // 6. Drain reports until we see the one created above. The watcher's
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

#[tokio::test]
#[ignore = "requires running dockerd; opt in via --ignored"]
async fn auto_attach_route_container_to_ingress_network() {
    // 1. Skip silently if no Docker socket / daemon.
    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(_) => return,
    };
    if docker.ping().await.is_err() {
        return;
    }

    let ingress_network_existed = docker
        .inspect_network(SHARED_PROXY_NETWORK, None::<InspectNetworkOptions<String>>)
        .await;
    let ingress_network_existed = ingress_network_existed.is_ok();

    let container_name = format!("{AUTOATTACH_TEST_NAME_PREFIX}{}", std::process::id());

    // 2. Remove stale containers from previous interrupted runs, even when
    //    their PID-based names differ from the current run.
    cleanup_autoattach_test_containers(&docker).await;

    // 3. Pull busybox if needed (often local already; ignore stream errors).
    let mut pull = docker.create_image(
        Some(CreateImageOptions::<&str> {
            from_image: "busybox:latest",
            ..Default::default()
        }),
        None,
        None,
    );
    while pull.next().await.is_some() {}

    let mut labels: HashMap<String, String> = HashMap::new();
    labels.insert("isengard.expose".to_string(), "autoattach.test".to_string());
    labels.insert("isengard.expose.port".to_string(), "80".to_string());
    labels.insert(
        AUTOATTACH_TEST_LABEL_KEY.to_string(),
        AUTOATTACH_TEST_LABEL_VALUE.to_string(),
    );

    let cont = docker
        .create_container(
            Some(CreateContainerOptions {
                name: container_name.clone(),
                platform: None,
            }),
            Config {
                image: Some("busybox:latest".to_string()),
                cmd: Some(vec!["sleep".to_string(), "30".to_string()]),
                labels: Some(labels),
                ..Default::default()
            },
        )
        .await
        .expect("create container");
    let start_result = docker.start_container::<String>(&cont.id, None).await;
    if let Err(e) = start_result {
        cleanup_autoattach_test_containers(&docker).await;
        panic!("start container: {e}");
    }

    let backend_state_dir = tempdir().unwrap();
    let endpoint_result = match BollardBackend::from_env(backend_state_dir.path()).await {
        Ok(backend) => backend
            .ensure_ingress_attachment(&container_name)
            .await
            .map_err(|e| format!("ensure_ingress_attachment: {e}")),
        Err(e) => Err(format!("BollardBackend::from_env: {e}")),
    };
    let inspect_result = docker.inspect_container(&container_name, None).await;

    // Cleanup before assertions so endpoint regressions do not leave the test
    // container running or a test-created ingress network behind.
    cleanup_autoattach_test_containers(&docker).await;
    if !ingress_network_existed {
        let _ = docker.remove_network(SHARED_PROXY_NETWORK).await;
    }

    let endpoint = endpoint_result.expect("ensure_ingress_attachment");
    assert!(
        matches!(
            endpoint,
            IngressEndpoint::Ready {
                mode: IngressEndpointMode::IsengardNetwork,
                ..
            }
        ),
        "expected isengard network endpoint, got {endpoint:?}"
    );

    let inspect = inspect_result.expect("inspect container after auto-attach");
    let ingress_ip = inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.networks.as_ref())
        .and_then(|nets| nets.get("isengard-proxy"))
        .and_then(|s| s.ip_address.as_ref())
        .filter(|ip| !ip.is_empty());
    assert!(
        ingress_ip.is_some(),
        "expected non-empty isengard-proxy NetworkSettings.Networks IPAddress"
    );
}
