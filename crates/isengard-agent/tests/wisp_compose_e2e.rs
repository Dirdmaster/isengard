//! Phase 0.6 wisp arc: end-to-end test that drives compose_apply
//! through the [`crate::runtime::RuntimeBackend`] trait against a real
//! [`WispBackend`].
//!
//! Mirrors the failed `isd deploy` demo flow that motivated the Phase
//! 0.6 refactor:
//!
//!   compose YAML
//!     -> reconcile_stack(&dyn RuntimeBackend, ...)
//!     -> WispBackend::create_container + start_container
//!     -> RuntimeBackend::list_containers / inspect_container project
//!        the running container back to the diff path
//!     -> a second reconcile is a NoChange noop
//!
//! `#[ignore]` because it needs:
//!   * root (cgroup writes, iptables, mount + clone3, bridge create),
//!   * Linux (cgroup v2, /proc, clone3, netlink),
//!   * network egress to pull busybox from Docker Hub.
//!
//! Run with (on the OrbStack `wisp` VM as root):
//!
//!   cargo test -p isengard-agent --test wisp_compose_e2e \
//!     -- --ignored --nocapture
//!
//! Cousin tests:
//!   * `wisp_backend_smoke.rs`: WispBackend CRUD only.
//!   * `wisp_labels_and_discovery.rs`: labels watcher + proxy discovery
//!     under WispBackend; this test is the load-bearing compose path.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use isengard_agent::compose_apply;
use isengard_agent::runtime::{ContainerState, RuntimeBackend, select_backend};

fn is_root() -> bool {
    match std::process::Command::new("id").arg("-u").output() {
        Ok(out) => out.status.success() && out.stdout.trim_ascii() == b"0",
        Err(_) => false,
    }
}

#[tokio::test]
#[ignore = "needs root + linux + cgroup v2 + network egress"]
async fn wisp_compose_reconcile_starts_then_noops() {
    if !is_root() {
        eprintln!("skipping: needs root");
        return;
    }
    if std::env::var("WISP_OFFLINE").is_ok() {
        eprintln!("skipping: WISP_OFFLINE set");
        return;
    }

    // Stable state dir per run: same shape as wisp_labels_and_discovery.
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let state_dir = std::path::PathBuf::from(format!("/var/tmp/wisp-compose-e2e-{suffix}"));
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    // Force the wisp arm of select_backend.
    unsafe {
        std::env::set_var("ISENGARD_RUNTIME", "wisp");
    }
    let backend: Arc<dyn RuntimeBackend> = select_backend(&state_dir)
        .await
        .expect("select_backend wisp");
    assert_eq!(backend.name(), "wisp");

    // Single-service compose with the same shape `isd deploy` would
    // write to /etc/isengard/stacks/demo/compose.yaml. busybox + sleep
    // is hermetic (no nginx CAP requirements) and stays running long
    // enough for the second reconcile to find it.
    let stack = "compose-e2e";
    let yaml = r#"services:
  hello:
    image: docker.io/library/busybox:latest
    container_name: compose-e2e-hello
    command: ["/bin/sh", "-c", "sleep 60"]
    labels:
      isengard.expose: e2e.wisp.local
"#;

    // First reconcile: container is missing, plan should Start it.
    let (plan, outcomes) = compose_apply::reconcile_stack(backend.as_ref(), stack, yaml)
        .await
        .expect("reconcile_stack first");
    assert_eq!(plan.ops.len(), 1, "expected one op, got {plan:?}");
    let failures: Vec<&compose_apply::ApplyOutcome> =
        outcomes.iter().filter(|o| o.error.is_some()).collect();
    assert!(
        failures.is_empty(),
        "first reconcile had failures: {failures:?}"
    );

    // Wait for the container to reach Running. WispBackend sees it
    // immediately; the test polls inspect_container so the assertion
    // is robust against the lifecycle's brief Created window.
    let id = "compose-e2e-hello";
    let mut state = ContainerState::Created;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let snap = backend
            .inspect_container(id)
            .await
            .expect("inspect_container ok")
            .expect("snapshot Some");
        state = snap.state;
        // Verify the snapshot carries the labels Phase 0.6 added so the
        // diff path can see them. (env / port_bindings / restart fields.)
        assert_eq!(snap.stack.as_deref(), Some(stack));
        assert_eq!(snap.service.as_deref(), Some("hello"));
        assert_eq!(
            snap.labels.get("isengard.expose").map(String::as_str),
            Some("e2e.wisp.local"),
        );
        if state == ContainerState::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(state, ContainerState::Running, "container did not start");

    // Second reconcile: same compose, container is already running, plan
    // should be entirely NoChange. This is the load-bearing assertion:
    // it proves the trait-driven list_running_for_stack + drift detection
    // round-trip back to NoChange under wisp.
    let (plan2, outcomes2) = compose_apply::reconcile_stack(backend.as_ref(), stack, yaml)
        .await
        .expect("reconcile_stack second");
    let failures2: Vec<&compose_apply::ApplyOutcome> =
        outcomes2.iter().filter(|o| o.error.is_some()).collect();
    assert!(
        failures2.is_empty(),
        "second reconcile had failures: {failures2:?}"
    );
    assert!(
        plan2.is_noop(),
        "second reconcile was not a noop: {plan2:?}"
    );

    // Cleanup: remove the container + drop the backend off the tokio
    // context (wisp_image::Client owns a blocking reqwest runtime).
    let _ = backend.stop_container(id, 5).await;
    let _ = backend.remove_container(id, true).await;
    tokio::task::spawn_blocking(move || drop(backend))
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&state_dir);
}
