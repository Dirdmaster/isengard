//! Real-Docker e2e: triggers a blue-green deployment of nginx, asserts
//! that within ~3 minutes the deployment row reaches Done, the proxy
//! routes to the new green container, and the blue container is gone.
//!
//! Gated behind `#[ignore]` — run with
//! `cargo test -p isengard-agent --test deployment_blue_green_happy -- --ignored --nocapture`.
//! Requires a running Docker daemon.

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use isengard_agent::deployment::{DeploymentSupervisor, SupervisorOutcome, UpdateTrigger};
use isengard_agent::proxy::ProxyState;
use isengard_agent::proxy::upstreams::{Upstream, UpstreamState};
use isengard_core::NoopEmitter;
use isengard_storage::deployment::DeploymentState;
use isengard_storage::host::EnrollHost;
use isengard_storage::inventory::Inventory;
use isengard_storage::stack::{InsertStack, StackSource};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NGINX_IMAGE: &str = "nginx:alpine";

async fn pull(docker: &Docker, image: &str) {
    let mut s = docker.create_image(
        Some(CreateImageOptions {
            from_image: image,
            ..Default::default()
        }),
        None,
        None,
    );
    while s.next().await.is_some() {}
}

async fn cleanup(docker: &Docker, name: &str) {
    let _ = docker
        .stop_container(name, Some(StopContainerOptions { t: 1 }))
        .await;
    let _ = docker
        .remove_container(
            name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

#[tokio::test]
#[ignore = "requires running dockerd; opt in via --ignored"]
async fn blue_green_happy_path_drains_blue_destroys_blue_and_serves_green() {
    let docker = Arc::new(Docker::connect_with_local_defaults().expect("docker"));
    let inv = Inventory::open_in_memory().await.expect("inventory");
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-bg-test".into(),
            hostname: "h-bg-test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
            docker_version: "24.0".into(),
            fleet: "default".into(),
        })
        .await
        .unwrap();
    let stack_id = inv
        .insert_stack(InsertStack {
            host_id,
            name: "blog".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    pull(&docker, NGINX_IMAGE).await;
    let blue_name = "isengard-bg-test-blue";
    cleanup(&docker, blue_name).await;
    let blue = docker
        .create_container(
            Some(CreateContainerOptions {
                name: blue_name.to_string(),
                platform: None,
            }),
            Config {
                image: Some(NGINX_IMAGE.to_string()),
                healthcheck: Some(bollard::models::HealthConfig {
                    test: Some(vec![
                        "CMD".into(),
                        "wget".into(),
                        "-q".into(),
                        "-O-".into(),
                        "http://localhost/".into(),
                    ]),
                    interval: Some(1_000_000_000), // 1s in ns
                    timeout: Some(1_000_000_000),
                    retries: Some(3),
                    start_period: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("create blue");
    docker
        .start_container(&blue.id, None::<StartContainerOptions<String>>)
        .await
        .expect("start blue");
    let blue_inspect = docker.inspect_container(&blue.id, None).await.unwrap();
    // network_settings.ip_address is sometimes None on Docker Desktop;
    // fall back to the first per-network IP.
    let blue_ip = blue_inspect
        .network_settings
        .as_ref()
        .and_then(|s| s.ip_address.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            blue_inspect
                .network_settings
                .as_ref()
                .and_then(|s| s.networks.as_ref())
                .and_then(|nets| {
                    nets.values()
                        .find_map(|s| s.ip_address.clone().filter(|s| !s.is_empty()))
                })
        })
        .expect("blue should have an IP");

    // Insert a routing rule pointing to blue.
    use isengard_storage::routing_rule::{
        InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode,
    };
    let _rule = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: Some(stack_id),
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "blog.bg.test".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Edge,
            healthcheck_path: Some("/".into()),
            healthcheck_interval_secs: 5,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    // Pre-populate the proxy upstream registry with blue.
    let proxy_state = ProxyState::new();
    {
        let mut w = proxy_state.upstreams.write().await;
        w.set(
            "blog.bg.test",
            Upstream {
                container_id: blue.id.clone(),
                addr: format!("{blue_ip}:80").parse().unwrap(),
                healthy: true,
                health_path: Some("/".into()),
                health_interval: Duration::from_secs(5),
                consecutive_failures: 0,
                state: UpstreamState::Active,
            },
        );
    }

    let supervisor = DeploymentSupervisor::new(
        inv.clone(),
        docker.clone(),
        proxy_state.clone(),
        Arc::new(NoopEmitter),
    );

    let trigger = UpdateTrigger {
        container_id: blue.id.clone(),
        host_id,
        stack_id,
        service_name: "web".into(),
        blue_digest: "sha256:fake-blue".into(),
        green_digest: "sha256:fake-green".into(),
        image_ref: NGINX_IMAGE.into(),
        public_hostname: Some("blog.bg.test".into()),
        container_port: Some(80),
        health_path: Some("/".into()),
        has_healthcheck: true,
        rw_volume_mounts: vec![],
        label_strategy: None,
    };

    let outcome = supervisor
        .handle_update_trigger(trigger)
        .await
        .expect("dispatch");
    let deployment_id = match outcome {
        SupervisorOutcome::BlueGreenSpawned { deployment_id } => deployment_id,
        other => panic!("expected BlueGreenSpawned, got {other:?}"),
    };

    // Poll until Done or timeout. Production defaults: 120s healthcheck
    // deadline + 60s swap_grace + 5s drain_buffer = ~185s worst case;
    // give 200s of slack.
    let deadline = Instant::now() + Duration::from_secs(200);
    let final_state = loop {
        let d = inv.get_deployment(&deployment_id).await.unwrap().unwrap();
        if d.state.is_terminal() {
            break d;
        }
        if Instant::now() >= deadline {
            panic!(
                "deployment did not finish: state={:?} error={:?}",
                d.state, d.error
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(
        final_state.state,
        DeploymentState::Done,
        "expected Done, got {:?} error={:?}",
        final_state.state,
        final_state.error
    );

    // Assert proxy now routes to green (a different container_id than blue).
    let r = proxy_state.upstreams.read().await;
    let after = r.get("blog.bg.test").expect("upstream still present");
    assert_ne!(after.container_id, blue.id, "upstream still points at blue");
    drop(r);

    // Assert blue is gone.
    let blue_after = docker.inspect_container(&blue.id, None).await;
    assert!(
        blue_after.is_err(),
        "blue container should have been removed"
    );

    // Cleanup green.
    if let Some(green_id) = final_state.green_container {
        cleanup(&docker, &green_id).await;
    }
}
