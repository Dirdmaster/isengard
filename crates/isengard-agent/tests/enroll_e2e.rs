//! End-to-end Phase 2d test: spawn a real controller, run the agent, verify
//! enrollment lands on both sides (agent.json exists, controller inventory
//! has a row).
//!
//! Phase 2e made `run_agent` long-lived (blocks on ctrl_c). These tests now
//! `tokio::spawn(run_agent(...))` and assert via polling, then abort the
//! handle when the assertions are satisfied.
//!
//! NOTE(phase-14, task-11): the underlying enrollment flow now requires mTLS
//! plus a controller-minted token. The Phase 2 helpers (`seed_token` writing
//! to `enrollment.token.<X>` settings, `http://` URLs, bearer auth) no longer
//! match the wire surface. These tests stay in the tree as documentation of
//! the old invariants; they're `#[ignore]`d and superseded by the Phase 14
//! Task 15 e2e (`auth_e2e.rs`).

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use isengard_agent::{AgentOptions, run_agent};
use isengard_controller::{ControllerOptions, run_controller};
use tempfile::TempDir;

const TEST_TOKEN: &str = "test-token-2d";

async fn spawn_controller(state_dir: PathBuf) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // SAFETY: env::set_var is unsafe in Rust 2024. Set the token once before
    // any concurrent access; both controller and agent read this same env var.
    unsafe {
        std::env::set_var("ISENGARD_TOKEN", TEST_TOKEN);
    }

    tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir,
            config: serde_json::Value::Object(Default::default()),
            dns_zone: String::new(),
            dns_listen: "127.0.0.1:0".parse().unwrap(),
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

/// Pre-seed the bearer token's enrollment payload so the controller can
/// resolve the agent's fleet on enroll. After Phase 6b/wizard work, every
/// enroll requires a token whose payload was issued via POST /api/v1/hosts;
/// in tests we write the payload directly.
async fn seed_token(state_dir: &std::path::Path, token: &str, fleet: &str) {
    use isengard_storage::Inventory;
    let db = state_dir.join("isengard.db");
    // Wait for the controller to create the db file.
    for _ in 0..200 {
        if db.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let inv = Inventory::open(&db).await.expect("seed: open inventory");
    inv.set_setting(
        &format!("enrollment.token.{token}"),
        &serde_json::json!({ "fleet": fleet, "hostname": null }),
    )
    .await
    .expect("seed: set_setting");
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

#[tokio::test]
#[ignore = "phase-14 task-11: superseded by auth_e2e.rs (Task 15)"]
async fn agent_enrolls_writes_state_and_appears_in_inventory() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();

    let addr = spawn_controller(controller_state.path().to_path_buf()).await;
    seed_token(controller_state.path(), TEST_TOKEN, "test").await;

    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
        proxy_http_port: None,
        proxy_https_port: None,
        tls: None,
        enroll_token: None,
        bootstrap_trust: Default::default(),
        advertise_iface: None,
    };
    let agent_handle = tokio::spawn(run_agent(opts));

    // Wait for enroll to complete (agent.json appears).
    let state_path = agent_state.path().join("agent.json");
    let appeared = wait_for_state_file(&state_path, 5_000).await;
    assert!(appeared, "agent.json must exist within 5s after enroll");

    let body = std::fs::read_to_string(&state_path).unwrap();
    assert!(
        body.contains("agent_id"),
        "agent.json must contain agent_id key"
    );

    // Controller side: hosts table must contain one row.
    use isengard_storage::Inventory;
    let db_path = controller_state.path().join("isengard.db");
    let inv = Inventory::open(&db_path).await.expect("open inventory");
    let hosts = inv.list_hosts().await.expect("list hosts");
    assert_eq!(
        hosts.len(),
        1,
        "controller inventory should have exactly 1 host"
    );
    assert!(
        !hosts[0].fingerprint.is_empty(),
        "fingerprint should be set"
    );

    agent_handle.abort();
}

#[tokio::test]
#[ignore = "phase-14 task-11: superseded by auth_e2e.rs (Task 15)"]
async fn second_run_is_idempotent_no_re_enroll() {
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();
    let addr = spawn_controller(controller_state.path().to_path_buf()).await;
    seed_token(controller_state.path(), TEST_TOKEN, "test").await;

    let opts = AgentOptions {
        controller_url: format!("http://{addr}"),
        state_dir: agent_state.path().to_path_buf(),
        config: serde_json::Value::Object(Default::default()),
        proxy_http_port: None,
        proxy_https_port: None,
        tls: None,
        enroll_token: None,
        bootstrap_trust: Default::default(),
        advertise_iface: None,
    };

    // First run: spawn, wait for enroll, abort.
    let handle1 = tokio::spawn(run_agent(opts.clone()));
    let state_path = agent_state.path().join("agent.json");
    assert!(
        wait_for_state_file(&state_path, 5_000).await,
        "first run agent.json"
    );
    let body1 = std::fs::read_to_string(&state_path).unwrap();
    handle1.abort();
    let _ = handle1.await; // soak the abort

    // Second run: spawn, wait briefly to let the agent reach the sync loop
    // (which means it skipped enroll), then abort.
    let handle2 = tokio::spawn(run_agent(opts));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let body2 = std::fs::read_to_string(&state_path).unwrap();
    handle2.abort();
    let _ = handle2.await;

    assert_eq!(body1, body2, "second run should not change agent.json");

    // Controller still has exactly 1 host.
    use isengard_storage::Inventory;
    let inv = Inventory::open(&controller_state.path().join("isengard.db"))
        .await
        .unwrap();
    let hosts = inv.list_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1, "should still be 1 host after second run");
}
