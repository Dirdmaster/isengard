//! Real-Docker e2e: blue-green deployment where green's healthcheck
//! never passes. Asserts the deployment aborts within deadline_secs,
//! green is cleaned up, blue stays serving.
//!
//! Gated behind `#[ignore]` — run with
//! `cargo test -p isengard-agent --test deployment_blue_green_aborts_on_healthcheck -- --ignored --nocapture`.
//! Requires a running Docker daemon.
//!
//! NOTE: this test uses production defaults (120s healthcheck deadline +
//! ~5s abort cleanup), so wall time is ~130s. The 150s poll deadline
//! below provides a small buffer. Cargo runs e2e tests in parallel by
//! default; if running both this and `deployment_blue_green_happy`
//! together causes Docker pressure, pin to one thread:
//! `cargo test --test deployment_blue_green_aborts_on_healthcheck -- --ignored --nocapture --test-threads=1`.

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
use isengard_storage::routing_rule::{
    InsertRoutingRule, RoutingRuleSource, RoutingRuleState, TlsMode,
};
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
async fn blue_green_aborts_when_green_healthcheck_never_passes() {
    let docker = Arc::new(Docker::connect_with_local_defaults().expect("docker"));
    let inv = Inventory::open_in_memory().await.expect("inventory");
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-bg-abort".into(),
            hostname: "h-bg-abort".into(),
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
            name: "blog-abort".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    pull(&docker, NGINX_IMAGE).await;
    let blue_name = "isengard-bg-abort-blue";
    cleanup(&docker, blue_name).await;
    let blue = docker
        .create_container(
            Some(CreateContainerOptions {
                name: blue_name.to_string(),
                platform: None,
            }),
            Config {
                image: Some(NGINX_IMAGE.to_string()),
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

    // Routing rule with a path that nginx returns 404 for — we'll point the
    // healthcheck at it. HealthChecker.check_once returns false on non-2xx,
    // so green will never go healthy and the deployment must abort at the
    // 120s deadline.
    let _rule = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: Some(stack_id),
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "blog.bg.abort".into(),
            protocol: "http".into(),
            adapter: "none".into(),
            tls_mode: TlsMode::Edge,
            healthcheck_path: Some("/this-path-returns-404".into()),
            healthcheck_interval_secs: 5,
            auth: None,
            state: RoutingRuleState::Active,
            source: RoutingRuleSource::Ui,
            source_container_id: None,
            source_imported_from: None,
        })
        .await
        .unwrap();

    // Pre-populate proxy upstream registry with blue.
    let proxy_state = ProxyState::new();
    {
        let mut w = proxy_state.upstreams.write().await;
        w.set(
            "blog.bg.abort",
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
        blue_digest: "sha256:blue".into(),
        green_digest: "sha256:green".into(),
        image_ref: NGINX_IMAGE.into(),
        public_hostname: Some("blog.bg.abort".into()),
        container_port: Some(80),
        health_path: Some("/this-path-returns-404".into()),
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

    // Poll until Aborted (deadline default 120s + buffer).
    let deadline = Instant::now() + Duration::from_secs(150);
    let final_state = loop {
        let d = inv.get_deployment(&deployment_id).await.unwrap().unwrap();
        if d.state.is_terminal() {
            break d;
        }
        if Instant::now() >= deadline {
            panic!(
                "deployment did not abort: state={:?} error={:?}",
                d.state, d.error
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    assert_eq!(
        final_state.state,
        DeploymentState::Aborted,
        "expected Aborted, got {:?} error={:?}",
        final_state.state,
        final_state.error
    );
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("healthcheck_timeout"),
        "error should mention healthcheck_timeout, got {:?}",
        final_state.error
    );

    // Blue still serving (proxy upstream still points at blue's container_id).
    let r = proxy_state.upstreams.read().await;
    let still = r.get("blog.bg.abort").expect("upstream present");
    assert_eq!(still.container_id, blue.id, "blue should still be serving");
    drop(r);

    // Blue container still alive.
    assert!(
        docker.inspect_container(&blue.id, None).await.is_ok(),
        "blue container should still be alive after abort"
    );

    // Cleanup blue.
    cleanup(&docker, &blue.id).await;
}
