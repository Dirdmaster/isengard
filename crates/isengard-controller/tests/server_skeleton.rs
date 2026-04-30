//! Phase 2c integration tests: real Enroll handler + token auth.
//!
//! Spawns the controller library on an ephemeral port with a temp state dir,
//! exercises Enroll over a tonic client.

// tonic::Status is ~256 bytes — interceptor closures must return Result<_, Status>
// per tonic's API. The size lint isn't actionable here.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use isengard_controller::{ControllerOptions, run_controller};
use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use tempfile::TempDir;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const TEST_TOKEN: &str = "test-token-1234";

struct Harness {
    addr: SocketAddr,
    _state: TempDir,
}

async fn spawn_controller(state_dir: PathBuf) -> SocketAddr {
    // Pick an ephemeral port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // The middleware reads ISENGARD_TOKEN from process env. Set it for the
    // entire test process — fine because all tests in this file use the
    // same value.
    //
    // SAFETY: env::set_var is unsafe in Rust 2024. This is the canonical
    // pattern for setting test fixtures; we set the value once before any
    // server task is spawned, and all tests use the same value.
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

    // Wait for the server to bind.
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 10s");
}

async fn harness() -> Harness {
    let dir = TempDir::new().expect("temp dir");
    let addr = spawn_controller(dir.path().to_path_buf()).await;
    Harness { addr, _state: dir }
}

fn sample_request() -> EnrollRequest {
    EnrollRequest {
        fingerprint: "test-host.example".into(),
        hostname: "test-host".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        agent_version: "0.1.0-alpha".into(),
        docker_version: "27.4.0".into(),
    }
}

async fn channel_to(addr: SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("connect")
}

// =====================================================================

#[tokio::test]
async fn enroll_without_token_returns_unauthenticated() {
    let h = harness().await;
    let mut client = ControllerClient::connect(format!("http://{}", h.addr))
        .await
        .expect("connect");

    let err = client
        .enroll(sample_request())
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_wrong_token_returns_unauthenticated() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let token: MetadataValue<_> = "Bearer not-the-right-token".parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    });

    let err = client
        .enroll(sample_request())
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_valid_token_returns_agent_id_and_heartbeat() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let token: MetadataValue<_> = format!("Bearer {TEST_TOKEN}").parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    });

    let resp = client
        .enroll(sample_request())
        .await
        .expect("enroll")
        .into_inner();
    assert!(!resp.agent_id.is_empty(), "agent_id must be set");
    assert_eq!(resp.heartbeat_interval_secs, 10);
    assert!(resp.server_time_ms > 0);
}

#[tokio::test]
async fn enroll_with_duplicate_fingerprint_returns_already_exists() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let token: MetadataValue<_> = format!("Bearer {TEST_TOKEN}").parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    });

    let _first = client
        .enroll(sample_request())
        .await
        .expect("first must succeed");
    let err = client
        .enroll(sample_request())
        .await
        .expect_err("second must fail");
    assert_eq!(err.code(), Code::AlreadyExists, "got {err:?}");
}
