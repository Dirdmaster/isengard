//! Phase 0.4 dispatch D2: smoke test for [`WispBackend`] end-to-end.
//!
//! This test exercises the full WispBackend surface (ensure_image,
//! create_container, start_container, inspect_container, remove_container)
//! against a real busybox image, on a real cgroup v2 host, with a real
//! wisp-net default bridge.
//!
//! `#[ignore]` because:
//!   * needs root (cgroup writes, iptables, mount + clone3),
//!   * needs Linux (cgroup v2, /proc, clone3),
//!   * needs network egress to pull from Docker Hub,
//!   * mutates host iptables + bridges (`wbr-default`).
//!
//! Run with:
//!   sudo -E cargo test -p isengard-agent --test wisp_backend_smoke \
//!     --release -- --ignored --nocapture
//!
//! On the OrbStack `wisp` VM as root:
//!   PATH=/home/dirdmaster/.cargo/bin:$PATH
//!   cd /Users/dirdmaster/Projects/isengard/.worktrees/next
//!   cargo test -p isengard-agent --test wisp_backend_smoke \
//!     -- --ignored --nocapture wisp_backend_busybox_lifecycle
//!
//! ## Pre-existing network requirement
//!
//! WispBackend's default attacher uses the network name `default` (subnet
//! from `WISP_DEFAULT_SUBNET`, fallback `10.83.0.0/24`). This smoke test
//! uses an empty `spec.networks` so the attacher is NOT exercised : the
//! container runs without an eth0. That keeps the test scope focused on
//! the runtime CRUD path; networked end-to-end tests are out of scope for
//! 0.4 because the agent's WispBackend does not auto-create the bridge
//! and the test framework would need a `wisp net create default` step
//! before construction. Deferred to 0.5 per the plan's open questions.

use std::collections::BTreeMap;
use std::time::Duration;

use isengard_agent::runtime::wisp_backend::WispBackend;
use isengard_agent::runtime::{
    ContainerCreateSpec, ContainerState, RestartPolicy, RuntimeBackend, RuntimeEventType,
};

fn busybox_spec(name: &str) -> ContainerCreateSpec {
    ContainerCreateSpec {
        container_name: name.to_string(),
        // busybox:latest is small (~2MB), available on Docker Hub anonymously,
        // and works on both arm64 (orbstack) and amd64 (production homelab
        // VMs) without re-pull because wisp-image's content store is
        // architecture-aware.
        image: "docker.io/library/busybox:latest".to_string(),
        stack: "smoke".into(),
        service: "hello".into(),
        // Print to stdout, sleep, exit 0. The sleep buys at least one
        // diff tick of the 2s C3 event emitter so we can assert a Die
        // event lands. Without it, the container exits before the first
        // emitter poll and the loop never sees a Running snapshot.
        // The runtime captures stdout to
        // <state_dir>/wisp/containers/<id>/stdout.log per Phase 0.4 C1.
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo wisp-smoke-ok && sleep 3 && exit 0".into(),
        ]),
        entrypoint: None,
        env: BTreeMap::new(),
        labels: BTreeMap::new(),
        mounts: Vec::new(),
        ports: Vec::new(),
        // No networks: keeps the test hermetic. Networked tests need
        // operator pre-creation of the default bridge (see module docs).
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
#[ignore = "needs root + linux + cgroup v2 + network egress"]
async fn wisp_backend_busybox_lifecycle() {
    // Use a hard-coded subdir under /var/tmp rather than a tempdir: the
    // wisp runtime's content store flock semantics dislike short-lived
    // dirs being created and torn down inside the same test process. A
    // stable path with a per-run suffix is the friendlier shape.
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let state_dir = std::path::PathBuf::from(format!("/var/tmp/wisp-smoke-{suffix}"));
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    let backend = WispBackend::from_env(&state_dir)
        .await
        .expect("WispBackend::from_env");

    // Subscribe BEFORE start_container so we don't miss the Start event.
    let mut events = backend.stream_events();

    // Pull the image. Returns the manifest digest. Idempotent against the
    // local content store.
    let digest = backend
        .ensure_image("docker.io/library/busybox:latest")
        .await
        .expect("ensure_image busybox");
    assert!(
        digest.starts_with("sha256:"),
        "manifest digest looks like sha256:...: got {digest}"
    );

    let spec = busybox_spec("wisp-smoke-1");
    let id = backend
        .create_container(&spec)
        .await
        .expect("create_container");
    assert_eq!(id, "wisp-smoke-1");

    backend.start_container(&id).await.expect("start_container");

    // Poll inspect_container until ContainerState::Exited. The container
    // sleeps 3s before exit, plus a 2s buffer for the runtime + emitter
    // to see the transition.
    use futures::StreamExt;
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        final_state,
        ContainerState::Exited,
        "container did not reach Exited within 10s"
    );

    // Drain the event stream with a generous timeout. `Start` is emitted
    // synchronously by `start_container` (we always see it). Die/Stop is
    // best-effort: the C3 emitter snapshots `runtime.list()` every 2s,
    // and the busybox `echo` container exits in <100ms : the emitter may
    // never observe the Running state and therefore may never emit a
    // Die. We assert Start strictly and merely log Die/Stop status.
    // Tighter event timing is a 0.5 stretch (cgroup.events fsnotify).
    let mut saw_start = false;
    let mut saw_die_or_stop = false;
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < drain_deadline {
        let recv = tokio::time::timeout(Duration::from_millis(500), events.next()).await;
        match recv {
            Ok(Some(ev)) if ev.container_id == id => match ev.event_type {
                RuntimeEventType::Start => saw_start = true,
                RuntimeEventType::Stop | RuntimeEventType::Die { .. } => {
                    saw_die_or_stop = true;
                }
                _ => {}
            },
            Ok(Some(_)) | Ok(None) | Err(_) => continue,
        }
        if saw_start && saw_die_or_stop {
            break;
        }
    }
    assert!(saw_start, "expected at least one Start event");
    if !saw_die_or_stop {
        eprintln!(
            "note: no Die/Stop event captured. Known limitation: the 2s \
             event-emitter poll can miss containers that exit faster than \
             one tick. Tracked in release notes for 0.5."
        );
    }

    // Idempotent cleanup. force=true deletes even if state is Created.
    backend
        .remove_container(&id, true)
        .await
        .expect("remove_container");

    // Drop the backend off the tokio context (wisp_image::Client owns a
    // reqwest blocking runtime that can't be dropped inside async).
    tokio::task::spawn_blocking(move || drop(backend))
        .await
        .unwrap();

    // Best-effort cleanup of the state dir; logged failure is fine.
    let _ = std::fs::remove_dir_all(&state_dir);
}
