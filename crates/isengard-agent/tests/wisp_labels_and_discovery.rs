//! Phase 0.5 dispatch: integration test for the labels watcher + proxy
//! discovery refactor under WispBackend.
//!
//! Proves the homelab routing flow end-to-end against the wisp runtime:
//!
//!   compose label
//!     -> ContainerCreateSpec persisted by WispBackend
//!     -> RuntimeBackend::stream_events fires Start
//!     -> labels::watch builds a ContainerLabelsReport
//!     -> proxy::discovery::resolve_container_ip returns the bridge IP
//!
//! `#[ignore]` because it needs:
//!   * root (cgroup writes, iptables, mount + clone3, bridge create),
//!   * Linux (cgroup v2, /proc, clone3, netlink),
//!   * network egress to pull busybox from Docker Hub.
//!
//! Run with (on the OrbStack `wisp` VM as root):
//!
//!   cargo test -p isengard-agent --test wisp_labels_and_discovery \
//!     -- --ignored --nocapture
//!
//! Cousin test: `wisp_backend_smoke.rs` exercises CRUD only and skips
//! networking; this one exercises the full routing-discovery stack with a
//! real bridge (10.83.0.0/24) auto-created by WispBackend.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use isengard_agent::runtime::{ContainerCreateSpec, RestartPolicy, RuntimeBackend, select_backend};
use isengard_agent::{labels, proxy::discovery};
use isengard_proto::pb::{AgentMessage, agent_message};

/// Best-effort EUID check. Returns true if `id -u` prints `0`. We avoid
/// pulling libc into dev-deps just for `geteuid` because the agent crate
/// does not depend on libc directly.
fn is_root() -> bool {
    match std::process::Command::new("id").arg("-u").output() {
        Ok(out) => out.status.success() && out.stdout.trim_ascii() == b"0",
        Err(_) => false,
    }
}

#[tokio::test]
#[ignore = "needs root + linux + cgroup v2 + network egress"]
async fn labels_watch_and_discovery_round_trip_under_wisp() {
    if !is_root() {
        eprintln!("skipping: needs root");
        return;
    }
    if std::env::var("WISP_OFFLINE").is_ok() {
        eprintln!("skipping: WISP_OFFLINE set");
        return;
    }

    // Stable state dir per run: wisp's content store flock dislikes the
    // tempdir-then-drop dance the smoke test documents.
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let state_dir = std::path::PathBuf::from(format!("/var/tmp/wisp-labels-{suffix}"));
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    // Force the wisp arm of select_backend. SAFETY: tests are single-process
    // but cargo runs them in parallel by default; the backend selection is
    // read once at construction so a race between `set_var` and the second
    // assert is the only risk. We accept it for ignored tests.
    unsafe {
        std::env::set_var("ISENGARD_RUNTIME", "wisp");
    }

    let backend: Arc<dyn RuntimeBackend> = select_backend(&state_dir)
        .await
        .expect("select_backend wisp");
    assert_eq!(backend.name(), "wisp");

    // A container that:
    //   * carries an isengard.expose label (triggers labels watcher),
    //   * declares a network (forces WispBackend to auto-create wisp-default),
    //   * sleeps long enough for inspect+events to settle.
    //
    // We use busybox + sleep rather than nginx to sidestep nginx's
    // capability requirements (CAP_CHOWN / CAP_SETUID for `nginx -g
    // 'daemon off'`); cap-add wiring through ContainerCreateSpec is on a
    // separate dispatch. We do NOT need an HTTP listener: the test only
    // proves labels + discovery, not actual proxy traffic.
    let mut labels_map = BTreeMap::new();
    labels_map.insert("isengard.expose".into(), "test.wisp.local".into());
    labels_map.insert("isengard.expose.port".into(), "80".into());
    labels_map.insert("isengard.stack".into(), "demo".into());

    let spec = ContainerCreateSpec {
        container_name: "wisp-labels-test".into(),
        image: "docker.io/library/busybox:latest".into(),
        stack: "demo".into(),
        service: "web".into(),
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()]),
        entrypoint: None,
        env: BTreeMap::new(),
        labels: labels_map,
        mounts: vec![],
        ports: vec![],
        networks: vec!["wisp-default".into()],
        restart: RestartPolicy::No,
        healthcheck: None,
        user: None,
        working_dir: None,
        hostname: None,
        linux_resources: None,
        secrets: vec![],
    };

    // Spawn the labels watcher BEFORE create_container so the event stream
    // subscription is in place when Start fires.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentMessage>(64);
    let watcher_backend = backend.clone();
    let watcher = tokio::spawn(async move {
        let _ = labels::watch(watcher_backend, tx).await;
    });

    // Pull image + create + start.
    backend
        .ensure_image(&spec.image)
        .await
        .expect("ensure_image busybox");
    let id = backend
        .create_container(&spec)
        .await
        .expect("create_container");
    backend.start_container(&id).await.expect("start_container");

    // Wait for a ContainerLabelsReport that mentions our expose host. The
    // watcher's initial scan races the Start event; either path is fine,
    // we just want one report.
    let mut got_report = false;
    let mut got_id: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let recv = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        match recv {
            Ok(Some(msg)) => {
                if let Some(agent_message::Payload::ContainerLabelsReport(r)) = &msg.payload {
                    if r.labels.get("isengard.expose").map(String::as_str)
                        == Some("test.wisp.local")
                    {
                        got_report = true;
                        got_id = Some(r.container_id.clone());
                        break;
                    }
                }
            }
            Ok(None) => break,  // sender dropped: watcher exited
            Err(_) => continue, // timeout: keep polling until deadline
        }
    }
    assert!(
        got_report,
        "labels watcher did not emit a report containing test.wisp.local within 15s"
    );
    let report_id = got_id.unwrap();

    // Validate proxy discovery resolves the container's bridge IP via the
    // same backend. The labels report carries `container_id` (the wisp
    // container name); the proxy applies routing rules with that string,
    // and `resolve_container_ip` looks up the snapshot by it.
    let resolved = discovery::resolve_container_ip(backend.as_ref(), &report_id).await;
    let ip_str = resolved.expect("discovery did not resolve a container IP");
    let ip: std::net::IpAddr = ip_str.parse().expect("resolved IP not parseable");
    let octets = match ip {
        std::net::IpAddr::V4(v4) => v4.octets(),
        std::net::IpAddr::V6(_) => panic!("unexpected ipv6: {ip}"),
    };
    // wisp-default subnet defaults to 10.83.0.0/24 (`DEFAULT_NETWORK_SUBNET`
    // in wisp_backend.rs). Operators can override with WISP_DEFAULT_SUBNET;
    // this test does not set it, so we assert the default.
    assert_eq!(octets[0], 10, "expected 10.x.x.x; got {ip}");
    assert_eq!(octets[1], 83, "expected 10.83.x.x; got {ip}");

    // Cleanup: stop, remove, abort the watcher. force=true on remove
    // tolerates a slow stop.
    let _ = backend.stop_container(&id, 5).await;
    let _ = backend.remove_container(&id, true).await;
    watcher.abort();

    // Drop the backend off the tokio context (wisp_image::Client owns a
    // reqwest blocking runtime that can't be dropped inside async).
    tokio::task::spawn_blocking(move || drop(backend))
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&state_dir);
}
