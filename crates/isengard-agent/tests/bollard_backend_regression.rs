//! Phase 0.4 dispatch D2: regression test for [`BollardBackend`].
//!
//! Mirrors `tests/wisp_backend_smoke.rs` for the default backend so we can
//! prove the trait wiring did not regress the bollard path. Same hello-world
//! semantics: pull busybox, run `echo`, exit 0, assert the snapshot reaches
//! Exited.
//!
//! `#[ignore]` because the test needs:
//!   * a reachable docker socket (Docker Desktop / dockerd / orbstack),
//!   * network egress to pull busybox,
//!   * the ability to mutate dockerd's container set.
//!
//! Run with:
//!   cargo test -p isengard-agent --test bollard_backend_regression \
//!     -- --ignored --nocapture
//!
//! On any host with docker running. Skips silently if the docker socket
//! isn't reachable so CI can run unconditionally.

use std::collections::BTreeMap;
use std::time::Duration;

use isengard_agent::runtime::bollard_backend::BollardBackend;
use isengard_agent::runtime::{ContainerCreateSpec, ContainerState, RestartPolicy, RuntimeBackend};

fn busybox_spec(name: &str) -> ContainerCreateSpec {
    ContainerCreateSpec {
        container_name: name.to_string(),
        // Same image as the wisp smoke test so any rate-limit / pull bug
        // hits both paths the same way.
        image: "docker.io/library/busybox:latest".to_string(),
        stack: "smoke".into(),
        service: "hello".into(),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo bollard-regression-ok && exit 0".into(),
        ]),
        entrypoint: None,
        env: BTreeMap::new(),
        labels: BTreeMap::new(),
        mounts: Vec::new(),
        ports: Vec::new(),
        networks: Vec::new(),
        restart: RestartPolicy::No,
        healthcheck: None,
        user: None,
        working_dir: None,
        hostname: None,
        linux_resources: None,
        secrets: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "needs docker socket + network egress"]
async fn bollard_backend_busybox_lifecycle() {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let state_dir = std::path::PathBuf::from(format!("/tmp/bollard-regression-{suffix}"));
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    // Skip silently if we can't reach the docker socket: the test is
    // OS-portable but the runtime isn't.
    let backend = match BollardBackend::from_env(&state_dir).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "skipping bollard regression: docker not reachable ({e}). \
                 Run with a docker daemon (Docker Desktop, orbstack, dockerd) up."
            );
            return;
        }
    };

    // Quick liveness ping so we surface a friendly message instead of a
    // confusing pull failure when dockerd is half-up.
    if backend.docker().info().await.is_err() {
        eprintln!("skipping bollard regression: docker info failed");
        return;
    }

    let _digest = backend
        .ensure_image("docker.io/library/busybox:latest")
        .await
        .expect("ensure_image busybox");

    let spec = busybox_spec(&format!("bollard-regression-{suffix}"));
    let id = backend
        .create_container(&spec)
        .await
        .expect("create_container");
    backend.start_container(&id).await.expect("start_container");

    // Bollard's `echo + exit 0` busybox finishes within a second on a warm
    // engine. 10s budget is generous for cold daemons.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut final_state = ContainerState::Created;
    while std::time::Instant::now() < deadline {
        let snap = backend
            .inspect_container(&id)
            .await
            .expect("inspect_container")
            .expect("snapshot Some");
        final_state = snap.state;
        if final_state == ContainerState::Exited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        final_state,
        ContainerState::Exited,
        "container did not reach Exited within 10s"
    );

    backend
        .remove_container(&id, true)
        .await
        .expect("remove_container");

    let _ = std::fs::remove_dir_all(&state_dir);
}
