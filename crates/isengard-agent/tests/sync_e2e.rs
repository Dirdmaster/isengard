//! Phase 2e e2e: agent runs, sends heartbeats, controller's last_seen_at
//! advances visibly.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::time::Duration;

use isengard_agent::{AgentOptions, run_agent};
use isengard_controller::{ControllerOptions, run_controller};
use isengard_storage::Inventory;
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2e";

async fn spawn_controller(state_dir: std::path::PathBuf) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    unsafe {
        std::env::set_var("ISENGARD_TOKEN", TEST_TOKEN);
    }

    tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir,
            config: serde_json::Value::Object(Default::default()),
        })
        .await;
    });

    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 10s");
}

async fn wait_for_state_file(path: &std::path::Path, timeout_ms: u64) -> bool {
    let polls = (timeout_ms / 50).max(1);
    for _ in 0..polls {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_heartbeats_advance_last_seen_at() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();

    let addr = spawn_controller(controller_state.path().to_path_buf()).await;

    // Spawn the agent. It will enroll then open the sync stream and send
    // heartbeats. The handle leaks intentionally — test process exits at
    // function end.
    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    };
    let agent_handle = tokio::spawn(run_agent(opts));

    // Wait for enroll to complete (agent.json appears).
    let state_path = agent_state.path().join("agent.json");
    assert!(
        wait_for_state_file(&state_path, 5_000).await,
        "agent.json must exist within 5s after enroll"
    );

    // Open the inventory and capture the initial last_seen_at value.
    // Right after enroll, last_seen_at MAY be None (Heartbeat hasn't fired
    // yet) or already set (if a Heartbeat raced ahead).
    let inv = Inventory::open(&controller_state.path().join("isengard.db"))
        .await
        .expect("open inventory");
    let hosts = inv.list_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1);
    let initial_last_seen = hosts[0].last_seen_at;

    // Wait up to 15 seconds for last_seen_at to advance from its initial value.
    // Phase 2e hardcodes a 10s heartbeat interval, so the first Heartbeat lands
    // ~10s after Hello. We poll every 250ms.
    let mut advanced = false;
    for _ in 0..60 {
        let hosts = inv.list_hosts().await.unwrap();
        let now = hosts[0].last_seen_at;
        if now.is_some() && now != initial_last_seen {
            advanced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        advanced,
        "last_seen_at must advance after a heartbeat (waited 15s; initial={initial_last_seen:?})"
    );

    agent_handle.abort();
}
