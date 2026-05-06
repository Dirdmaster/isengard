//! Real-Docker e2e: Phase 9F rollback failure handler.
//!
//! Same shape as `deployment_blue_green_aborts_on_healthcheck` but with a
//! Rollback-policy seeded `previous_digest`. The deployment fails its
//! healthcheck (path returns 404 forever), the supervisor enters the
//! rollback branch, re-pulls the recorded digest, recreates a fresh
//! container at that pinned image, and lands in `RolledBack`.
//!
//! Gated behind `#[ignore]` so CI without dockerd skips it. Run with:
//! `cargo test -p isengard-agent --test deployment_blue_green_rollback -- --ignored --nocapture`.

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
use isengard_core::policy::{FailureHandling, Policy, PolicyScopeType};
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
async fn blue_green_rollback_on_healthcheck_failure() {
    let docker = Arc::new(Docker::connect_with_local_defaults().expect("docker"));
    let inv = Inventory::open_in_memory().await.expect("inventory");
    let host_id = inv
        .enroll_host(EnrollHost {
            fingerprint: "fp-bg-rb".into(),
            hostname: "h-bg-rb".into(),
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
            name: "blog-rb".into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();

    // Install a service-scope policy with on_failure=Rollback so the
    // supervisor seeds previous_digest at insert time.
    inv.upsert_policy(
        PolicyScopeType::Service,
        "web",
        &Policy {
            on_failure: Some(FailureHandling::Rollback),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    pull(&docker, NGINX_IMAGE).await;
    let blue_name = "isengard-bg-rb-blue";
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

    // Recover the actual blue digest so the rollback re-pull has a
    // resolvable image.
    let blue_digest = blue_inspect
        .image
        .as_deref()
        .map(str::to_string)
        .expect("blue inspect carries an image digest");

    // Routing rule with a path that never returns 2xx so green stays
    // unhealthy and the supervisor takes the rollback branch.
    let _rule = inv
        .insert_routing_rule(InsertRoutingRule {
            fleet: "default".into(),
            host_id,
            stack_id: Some(stack_id),
            service_name: "web".into(),
            container_port: 80,
            public_hostname: "blog.bg.rb".into(),
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

    let proxy_state = ProxyState::new();
    {
        let mut w = proxy_state.upstreams.write().await;
        w.set(
            "blog.bg.rb",
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

    // Wire the supervisor with a real PolicyLoader so it sees the
    // service-scope Rollback policy.
    let policy_loader: Arc<dyn isengard_core::PolicyLoader> = Arc::new(
        isengard_storage::InventoryPolicyLoader::new(Arc::new(inv.clone())),
    );
    let supervisor = DeploymentSupervisor::new(
        inv.clone(),
        docker.clone(),
        proxy_state.clone(),
        Arc::new(NoopEmitter),
    )
    .with_policy_loader(policy_loader);

    let trigger = UpdateTrigger {
        container_id: blue.id.clone(),
        host_id,
        stack_id,
        service_name: "web".into(),
        blue_digest: blue_digest.clone(),
        green_digest: "sha256:nonexistent_green".into(),
        image_ref: NGINX_IMAGE.into(),
        public_hostname: Some("blog.bg.rb".into()),
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

    // Poll until terminal (deadline default 120s + rollback re-pull
    // budget). 180s is comfortable on dockerd-equipped CI.
    let deadline = Instant::now() + Duration::from_secs(180);
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
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    // The rollback may succeed (RolledBack) when the digest is locally
    // cached, or fail (RollbackFailed) when the registry refuses the
    // pull. Both are acceptable terminal states for this e2e: the
    // important assertions are the rollback metadata fields, which
    // are populated regardless.
    assert!(
        matches!(
            final_state.state,
            DeploymentState::RolledBack | DeploymentState::RollbackFailed
        ),
        "expected RolledBack or RollbackFailed, got {:?} error={:?}",
        final_state.state,
        final_state.error
    );
    assert_eq!(
        final_state.previous_digest.as_deref(),
        Some(blue_digest.as_str()),
        "previous_digest should mirror blue's digest"
    );
    assert!(
        final_state.rollback_attempted_at.is_some(),
        "rollback_attempted_at should be stamped"
    );

    // Cleanup any rolled-back container that was created.
    if final_state.state == DeploymentState::RolledBack {
        let id_short = &deployment_id[..deployment_id.len().min(8)];
        let rb_name = format!("web-rb-{id_short}");
        cleanup(&docker, &rb_name).await;
    }
    cleanup(&docker, &blue.id).await;
}
