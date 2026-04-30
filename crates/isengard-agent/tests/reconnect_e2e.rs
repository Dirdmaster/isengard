//! Phase 2f e2e: agent survives controller restart.
//!
//! Strategy: spawn controller A on a fixed port, let agent enroll + heartbeat
//! for a bit, abort controller A, spawn controller B on the SAME port (with
//! the SAME state dir, so it has the agent's host row), wait for agent to
//! reconnect, verify last_seen_at advances after the gap.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use isengard_agent::{AgentOptions, run_agent};
use isengard_controller::{ControllerOptions, run_controller};
use isengard_storage::Inventory;
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2f";

/// Spawn a controller on a SPECIFIC port (so a restart can re-bind).
/// Returns a JoinHandle so the caller can `.abort()` to "kill" the controller.
async fn spawn_controller_on(
    port: u16,
    state_dir: PathBuf,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    unsafe {
        std::env::set_var("ISENGARD_TOKEN", TEST_TOKEN);
    }

    let handle = tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir,
            config: serde_json::Value::Object(Default::default()),
        })
        .await;
    });

    // Wait for bind.
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return (addr, handle);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

/// Pick a free port by binding 0, then drop the listener.
fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_survives_controller_restart() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();
    let port = pick_free_port();

    // Spawn controller A.
    let (addr, ctrl_a) = spawn_controller_on(port, controller_state.path().to_path_buf()).await;

    // Spawn the agent against `addr`.
    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
    };
    let agent_handle = tokio::spawn(run_agent(opts));

    // Wait for enrollment.
    let state_path = agent_state.path().join("agent.json");
    for _ in 0..50 {
        if state_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(state_path.exists(), "agent should enroll within 5s");

    // Wait for the first heartbeat to land (last_seen_at non-null).
    let inv_path = controller_state.path().join("isengard.db");
    let mut first_last_seen = None;
    for _ in 0..120 {
        let inv = Inventory::open(&inv_path).await.unwrap();
        let hosts = inv.list_hosts().await.unwrap();
        if let Some(ts) = hosts.first().and_then(|h| h.last_seen_at) {
            first_last_seen = Some(ts);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let initial_ts = first_last_seen.expect("first heartbeat must land within 30s");

    // Kill controller A. Agent's stream will error; reconnect loop kicks in.
    ctrl_a.abort();
    let _ = ctrl_a.await;
    // Give the OS a moment to release the port.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Spawn controller B on the same port + same state dir. Same agent_id
    // already in the hosts table.
    let (_addr_b, _ctrl_b) = spawn_controller_on(port, controller_state.path().to_path_buf()).await;

    // Wait up to 30s for last_seen_at to advance from `initial_ts`. The agent
    // backoff starts at ~1s; reconnection should land in ~1-3s, then the next
    // heartbeat fires up to 10s later, then touch_host runs.
    let mut advanced = false;
    for _ in 0..120 {
        let inv = Inventory::open(&inv_path).await.unwrap();
        let hosts = inv.list_hosts().await.unwrap();
        if let Some(ts) = hosts.first().and_then(|h| h.last_seen_at) {
            if ts > initial_ts {
                advanced = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        advanced,
        "agent should reconnect to controller B and advance last_seen_at within 30s"
    );

    agent_handle.abort();
}
