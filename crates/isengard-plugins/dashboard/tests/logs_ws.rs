//! Phase 13B: integration tests for the WebSocket logs endpoint
//! `GET /api/v1/services/:stack_id/:service_name/logs/ws`.
//!
//! Strategy: spin up an axum::serve on an ephemeral TCP port, register a
//! synthetic Sync sender on the routing pusher (no real agent), connect a
//! tungstenite client to the WebSocket, push synthetic `LogChunk`s through
//! the controller's `LogFanout`, and assert the JSON frames the client
//! receives.
//!
//! The agent-side bollard tail is exercised by the agent crate's unit tests;
//! here we cover the controller -> WebSocket boundary.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
// SinkExt is used via the close() trait method on the WebSocket stream below.
#[allow(unused_imports)]
use futures_util::SinkExt;
use futures_util::StreamExt;
use isengard_controller::ControllerHandles;
use isengard_controller::bus::EventBus;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::log_fanout::LogFanout;
use isengard_controller::revocation::RevocationSet;
use isengard_controller::routing::RoutingPusher;
use isengard_plugin_dashboard::ws as dashboard_ws;
use isengard_proto::pb::{ControllerMessage, LogChunk, log_chunk};
use isengard_storage::inventory::Inventory;
use isengard_storage::journal::Journal;
use isengard_storage::{
    EnrollHost, HostId, InsertService, InsertStack, ServiceState, StackId, StackSource,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

async fn make_handles() -> Arc<ControllerHandles> {
    let inv = Arc::new(Inventory::open_in_memory().await.unwrap());
    let journal = Arc::new(Journal::open_in_memory().await.unwrap());
    let bus = Arc::new(EventBus::new());
    let routing = Arc::new(RoutingPusher::new(inv.clone()));
    let ca = Arc::new(Authority::load_or_init(&inv).await.unwrap());
    let enrollment = Arc::new(EnrollmentService::new(inv.clone(), ca.clone()));
    let revocation = RevocationSet::load_from_inventory(&inv).await.unwrap();
    let secrets = std::sync::Arc::new(isengard_controller::secrets::SecretsStore::new(
        inv.clone(),
        None,
    ));
    Arc::new(ControllerHandles {
        inventory: inv,
        journal,
        bus,
        routing,
        enrollment,
        revocation,
        db_path: std::path::PathBuf::from(":memory:"),
        log_fanout: LogFanout::new(),
        compose_broker: std::sync::Arc::new(
            isengard_controller::compose_broker::ComposeBroker::new(),
        ),
        secrets,
    })
}

async fn enroll(handles: &ControllerHandles, hostname: &str) -> HostId {
    handles
        .inventory
        .enroll_host(EnrollHost {
            fingerprint: format!("fp-{hostname}"),
            hostname: hostname.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0.1".into(),
            docker_version: "27".into(),
            fleet: "test".into(),
        })
        .await
        .unwrap()
}

async fn seed_service(
    handles: &ControllerHandles,
    host: HostId,
    stack_name: &str,
    service_name: &str,
) -> StackId {
    let stack_id = handles
        .inventory
        .insert_stack(InsertStack {
            host_id: host,
            name: stack_name.into(),
            source: StackSource::Compose,
        })
        .await
        .unwrap();
    handles
        .inventory
        .insert_service(InsertService {
            host_id: host,
            stack_id: Some(stack_id),
            name: service_name.into(),
            image: "img:1".into(),
            state: ServiceState::Running,
        })
        .await
        .unwrap();
    stack_id
}

/// Stand up the WebSocket-only sub-router on an ephemeral port. Returns the
/// local URL prefix (`ws://127.0.0.1:PORT`).
async fn serve(handles: Arc<ControllerHandles>) -> String {
    let app = Router::new()
        .route(
            "/api/v1/services/{stack_id}/{service_name}/logs/ws",
            get(dashboard_ws::handle_service_logs),
        )
        .with_state(handles);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{addr}")
}

/// Register a fake outbound Sync sender on the routing pusher for `host`. The
/// returned receiver lets the test assert that `StartLogStream` /
/// `StopLogStream` ControllerMessages were sent. Drop the receiver to drop
/// the registration (mirrors agent disconnect).
async fn fake_agent(
    handles: &ControllerHandles,
    host: HostId,
) -> mpsc::Receiver<Result<ControllerMessage, tonic::Status>> {
    let (tx, rx) = mpsc::channel(16);
    handles.routing.register_sender(host, tx).await;
    rx
}

fn line_chunk(sub: &str, msg: &str) -> LogChunk {
    LogChunk {
        subscription_id: sub.into(),
        kind: log_chunk::Kind::Line as i32,
        occurred_at: "2026-05-06T14:00:00Z".into(),
        stream: "stdout".into(),
        line: msg.into(),
        dropped: 0,
        reason: String::new(),
    }
}

fn backfill_chunk(sub: &str, msg: &str) -> LogChunk {
    LogChunk {
        subscription_id: sub.into(),
        kind: log_chunk::Kind::Backfill as i32,
        occurred_at: "2026-05-06T13:59:59Z".into(),
        stream: "stdout".into(),
        line: msg.into(),
        dropped: 0,
        reason: String::new(),
    }
}

/// Read the StartLogStream subscription_id off the fake-agent receiver. The
/// dashboard sends this to every host before draining chunks.
async fn read_subscription_id(
    rx: &mut mpsc::Receiver<Result<ControllerMessage, tonic::Status>>,
) -> String {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("StartLogStream not sent")
        .expect("agent receiver closed")
        .expect("status err");
    match msg.payload {
        Some(isengard_proto::pb::controller_message::Payload::StartLogStream(s)) => {
            s.subscription_id
        }
        other => panic!("expected StartLogStream, got {:?}", other),
    }
}

async fn next_text_frame(
    sock: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<serde_json::Value> {
    while let Some(msg) = sock.next().await {
        match msg {
            Ok(WsMessage::Text(t)) => {
                return Some(serde_json::from_str(&t).unwrap());
            }
            Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => continue,
            Ok(WsMessage::Close(_)) | Err(_) => return None,
            _ => continue,
        }
    }
    None
}

#[tokio::test]
async fn websocket_handshake_succeeds() {
    let handles = make_handles().await;
    let host = enroll(&handles, "h1").await;
    let stack_id = seed_service(&handles, host, "blog", "web").await;
    let _agent = fake_agent(&handles, host).await;

    let url = serve(handles).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let res = tokio_tungstenite::connect_async(&conn_url).await;
    assert!(res.is_ok(), "handshake failed: {:?}", res.err());
}

#[tokio::test]
async fn backfill_then_live_lines_reach_client() {
    let handles = make_handles().await;
    let host = enroll(&handles, "h1").await;
    let stack_id = seed_service(&handles, host, "blog", "web").await;
    let mut agent_rx = fake_agent(&handles, host).await;

    let url = serve(handles.clone()).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn_url).await.unwrap();

    // The dashboard sends StartLogStream to our fake agent. Capture sub_id.
    let sub = read_subscription_id(&mut agent_rx).await;

    // Push a backfill chunk + a live line through the fanout.
    handles
        .log_fanout
        .route(backfill_chunk(&sub, "starting"))
        .await;
    handles
        .log_fanout
        .route(line_chunk(&sub, "hello world"))
        .await;

    let f1 = next_text_frame(&mut ws).await.expect("first frame");
    let f2 = next_text_frame(&mut ws).await.expect("second frame");
    assert_eq!(f1["type"], "backfill");
    assert_eq!(f2["type"], "line");
    assert_eq!(f2["msg"], "hello world");
}

#[tokio::test]
async fn dropped_frame_renders() {
    let handles = make_handles().await;
    let host = enroll(&handles, "h1").await;
    let stack_id = seed_service(&handles, host, "blog", "web").await;
    let mut agent_rx = fake_agent(&handles, host).await;

    let url = serve(handles.clone()).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn_url).await.unwrap();
    let sub = read_subscription_id(&mut agent_rx).await;

    let drop = LogChunk {
        subscription_id: sub,
        kind: log_chunk::Kind::Dropped as i32,
        occurred_at: String::new(),
        stream: String::new(),
        line: String::new(),
        dropped: 47,
        reason: "backpressure".into(),
    };
    handles.log_fanout.route(drop).await;

    let f = next_text_frame(&mut ws).await.expect("frame");
    assert_eq!(f["type"], "dropped");
    assert_eq!(f["count"], 47);
}

#[tokio::test]
async fn unavailable_emitted_when_no_agent_registered() {
    let handles = make_handles().await;
    let host = enroll(&handles, "h1").await;
    let stack_id = seed_service(&handles, host, "blog", "web").await;
    // Note: NO fake_agent registration -> send_to_host returns false.

    let url = serve(handles).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn_url).await.unwrap();

    let f1 = next_text_frame(&mut ws).await.expect("frame");
    assert_eq!(f1["type"], "unavailable");
    let f2 = next_text_frame(&mut ws).await.expect("frame");
    // After the unavailable frame the controller emits a `closed` and shuts down.
    assert_eq!(f2["type"], "closed");
}

#[tokio::test]
async fn multi_host_aggregation_interleaves() {
    let handles = make_handles().await;
    let h1 = enroll(&handles, "prod-04").await;
    let h2 = enroll(&handles, "prod-05").await;
    let stack_id = seed_service(&handles, h1, "blog", "web").await;
    seed_service(&handles, h2, "blog", "web").await;
    let mut agent1_rx = fake_agent(&handles, h1).await;
    let mut agent2_rx = fake_agent(&handles, h2).await;

    let url = serve(handles.clone()).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn_url).await.unwrap();

    // Both hosts get StartLogStream with the same sub id.
    let sub1 = read_subscription_id(&mut agent1_rx).await;
    let sub2 = read_subscription_id(&mut agent2_rx).await;
    assert_eq!(sub1, sub2);

    // Push a chunk from each host.
    handles.log_fanout.route(line_chunk(&sub1, "from-h1")).await;
    handles.log_fanout.route(line_chunk(&sub2, "from-h2")).await;

    let f1 = next_text_frame(&mut ws).await.expect("frame");
    let f2 = next_text_frame(&mut ws).await.expect("frame");
    let texts: Vec<String> = [
        f1["msg"].as_str().unwrap_or("").to_string(),
        f2["msg"].as_str().unwrap_or("").to_string(),
    ]
    .into_iter()
    .collect();
    assert!(texts.iter().any(|t| t == "from-h1"));
    assert!(texts.iter().any(|t| t == "from-h2"));
}

#[tokio::test]
async fn client_disconnect_unregisters_subscription_and_sends_stop() {
    let handles = make_handles().await;
    let host = enroll(&handles, "h1").await;
    let stack_id = seed_service(&handles, host, "blog", "web").await;
    let mut agent_rx = fake_agent(&handles, host).await;

    let url = serve(handles.clone()).await;
    let conn_url = format!("{url}/api/v1/services/{}/web/logs/ws", stack_id.0);
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn_url).await.unwrap();
    let sub = read_subscription_id(&mut agent_rx).await;

    // Push one chunk to confirm the pipe is up.
    handles.log_fanout.route(line_chunk(&sub, "alive")).await;
    let _ = next_text_frame(&mut ws).await;

    // Client disconnect.
    let _ = ws.close(None).await;
    drop(ws);

    // The dashboard should send a StopLogStream and unregister the fanout.
    let mut saw_stop = false;
    for _ in 0..10 {
        if let Ok(Some(Ok(msg))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), agent_rx.recv()).await
        {
            if matches!(
                msg.payload,
                Some(isengard_proto::pb::controller_message::Payload::StopLogStream(_))
            ) {
                saw_stop = true;
                break;
            }
        }
    }
    assert!(saw_stop, "expected StopLogStream after client disconnect");

    // Give the dashboard a moment to call unregister.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // After unregister, routing a chunk for this sub should be a no-op.
    let outcome = handles.log_fanout.route(line_chunk(&sub, "ghost")).await;
    use isengard_controller::log_fanout::Outcome;
    assert_eq!(outcome, Outcome::Unknown);
}
