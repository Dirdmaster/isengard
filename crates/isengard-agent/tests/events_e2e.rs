//! E2e: an event sent on the agent→controller Sync stream lands in
//! the controller's `events` journal (and would have been broadcast on the
//! bus, though this test only asserts the persistence side).
//!
//! Approach: spin up the controller in-process, run the real agent long
//! enough for it to enroll, then read `agent.json` for the agent_id. With
//! that id, open a *second* raw `ControllerClient` Sync stream from the test
//! itself — performing the SyncHello + an `AgentMessage::Event` directly.
//! Wait up to 2s, then open the controller's SQLite file via `Journal::open`
//! and assert a row with `kind == "test.synthetic"` is present.
//!
//! This deliberately bypasses `run_agent`'s internal `OutboundEmitter` because
//! `AgentOptions` exposes no hook to inject one; the wire path under test is
//! exactly the path real plugin events take (Event payload variant → Sync
//! handler → Journal → EventBus). The agent's internal emitter is unit-tested
//! in `events::tests` separately.
//!
//! NOTE(phase-14, task-11): superseded by auth_e2e.rs (Task 15). The
//! bootstrap (`http://`, bearer token, `seed_token` writing settings rows)
//! does not match the mTLS surface; left in tree as documentation and marked
//! `#[ignore]`.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use isengard_agent::{AgentOptions, run_agent};
use isengard_controller::{ControllerOptions, run_controller};
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_proto::pb::{AgentMessage, Event as ProtoEvent, SyncHello, agent_message};
use isengard_storage::Journal;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

const TEST_TOKEN: &str = "test-token-4b";

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
            acme: Default::default(),
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

async fn seed_token(state_dir: &std::path::Path, token: &str, fleet: &str) {
    use isengard_storage::Inventory;
    let db = state_dir.join("isengard.db");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "phase-14 task-11: superseded by auth_e2e.rs (Task 15)"]
async fn agent_emitted_event_lands_in_controller_journal() {
    tokio::time::timeout(Duration::from_secs(30), run_test())
        .await
        .expect("test exceeded 30s");
}

async fn run_test() {
    // -- 1. Spin up controller + run agent so it enrolls. -------------------
    let controller_state = TempDir::new().unwrap();
    let agent_state = TempDir::new().unwrap();

    let addr = spawn_controller(controller_state.path().to_path_buf()).await;
    seed_token(controller_state.path(), TEST_TOKEN, "test").await;

    let agent_url = format!("http://{addr}");
    let opts = AgentOptions {
        controller_url: agent_url.clone(),
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
    assert!(
        wait_for_state_file(&state_path, 5_000).await,
        "agent.json must exist within 5s after enroll",
    );

    // Read the agent_id the controller assigned.
    let body = std::fs::read_to_string(&state_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let agent_id = parsed["agent_id"]
        .as_str()
        .expect("agent.json should contain agent_id")
        .to_string();

    // -- 2. Open a raw Sync stream from this test as the same agent. --------
    //
    // The controller doesn't gate per-agent stream uniqueness; opening a
    // second Sync stream with the same agent_id is fine for v1. We use it as
    // an injection point to push an Event frame onto the wire.
    let channel = Channel::from_shared(agent_url)
        .unwrap()
        .connect()
        .await
        .expect("connecting to controller");
    let bearer: MetadataValue<_> = format!("Bearer {TEST_TOKEN}").parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let (tx, rx) = mpsc::channel::<AgentMessage>(8);
    let outbound = ReceiverStream::new(rx);

    // Hello first.
    tx.send(AgentMessage {
        payload: Some(agent_message::Payload::Hello(SyncHello {
            agent_id: agent_id.clone(),
        })),
    })
    .await
    .unwrap();

    // Open the bidi stream.
    let response = client.sync(Request::new(outbound)).await.expect("Sync RPC");
    let _inbound = response.into_inner();

    // Now push the synthetic event.
    let synthetic = ProtoEvent {
        kind: "test.synthetic".to_string(),
        occurred_at: Utc::now().to_rfc3339(),
        summary: "from test".to_string(),
        container_name: None,
        image: None,
        old_digest: None,
        new_digest: None,
        error: None,
        metadata_json: None,
    };
    tx.send(AgentMessage {
        payload: Some(agent_message::Payload::Event(synthetic)),
    })
    .await
    .unwrap();

    // -- 3. Poll the journal up to 2s for the event. ------------------------
    let db_path = controller_state.path().join("isengard.db");
    let journal = Journal::open(&db_path).await.expect("open journal");

    let mut found = false;
    for _ in 0..40 {
        let rows = journal.list_recent(50).await.expect("list_recent");
        if rows.iter().any(|r| r.kind == "test.synthetic") {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "synthetic event should land in controller journal within 2s",
    );

    // Sanity: the row should have the agent's host_id stamped on it (the Sync
    // handler resolves host_id from the SyncHello and overwrites whatever the
    // proto carried).
    let rows = journal.list_recent(50).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.kind == "test.synthetic")
        .expect("row exists");
    assert_eq!(row.summary, "from test");
    assert!(
        row.host_id.is_some(),
        "host_id should be set by the Sync handler",
    );

    // -- 4. Cleanup. --------------------------------------------------------
    drop(tx); // close our outbound; controller's spawned task will see EOF
    agent_handle.abort();
    let _ = agent_handle.await;
}
