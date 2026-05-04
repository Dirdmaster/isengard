//! Phase 2c integration tests for the controller's gRPC surface, refreshed
//! for Phase 14: the `Enroll` request/response now carry an enrollment
//! token and a cert bundle (Task 6 of Phase 14).
//!
//! These tests still cover the bearer-token middleware (`TokenAuthLayer`),
//! which Task 8 will replace with mTLS. Until then, the middleware is still
//! the outer gate and rejects unauthenticated calls before they reach the
//! handler.

// tonic::Status is ~256 bytes — interceptor closures must return Result<_, Status>
// per tonic's API. The size lint isn't actionable here.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

use chrono::Duration;
use isengard_controller::ca::Authority;
use isengard_controller::enrollment::EnrollmentService;
use isengard_controller::{ControllerOptions, run_controller};
use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use isengard_storage::Inventory;
use isengard_storage::enrollment_token::TokenRole;
use tempfile::TempDir;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const TEST_TOKEN: &str = "test-token-1234";

struct Harness {
    addr: SocketAddr,
    state: TempDir,
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
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 10s");
}

async fn harness() -> Harness {
    let dir = TempDir::new().expect("temp dir");
    let addr = spawn_controller(dir.path().to_path_buf()).await;
    Harness { addr, state: dir }
}

/// Mint a fresh enrollment token via the same on-disk SQLite file the
/// running controller is using. We attach a second `Inventory` handle and
/// build an `EnrollmentService` over it; SQLite's WAL mode lets the
/// controller and the test share the file safely.
async fn mint_enrollment_token(state: &TempDir) -> String {
    let db = state.path().join("isengard.db");
    for _ in 0..200 {
        if db.exists() {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    let inv = std::sync::Arc::new(Inventory::open(&db).await.expect("open inventory"));
    let ca = std::sync::Arc::new(Authority::load_or_init(&inv).await.expect("load ca"));
    let svc = EnrollmentService::new(inv, ca);
    svc.mint(TokenRole::Agent, Duration::minutes(5))
        .await
        .expect("mint enrollment token")
}

fn sample_request(token: String) -> EnrollRequest {
    EnrollRequest {
        token,
        hostname: "test-host".into(),
        os: "linux".into(),
        version: "0.1.0-alpha".into(),
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
async fn enroll_without_bearer_returns_unauthenticated() {
    let h = harness().await;
    let mut client = ControllerClient::connect(format!("http://{}", h.addr))
        .await
        .expect("connect");

    // No bearer token on the request — the TokenAuthLayer middleware rejects
    // before the handler runs. Body content is irrelevant; we still send a
    // syntactically valid request so the test fails for the right reason.
    let err = client
        .enroll(sample_request("placeholder".into()))
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_wrong_bearer_returns_unauthenticated() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let token: MetadataValue<_> = "Bearer not-the-right-token".parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    });

    let err = client
        .enroll(sample_request("placeholder".into()))
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}

#[tokio::test]
async fn enroll_with_valid_bearer_and_token_returns_cert_bundle() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let bearer: MetadataValue<_> = format!("Bearer {TEST_TOKEN}").parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let enrollment_token = mint_enrollment_token(&h.state).await;
    let resp = client
        .enroll(sample_request(enrollment_token))
        .await
        .expect("enroll")
        .into_inner();

    assert_eq!(resp.host_id.len(), 16, "host_id must be 16-byte ULID");
    assert!(
        resp.agent_cert_pem.contains("BEGIN CERTIFICATE"),
        "agent cert PEM expected"
    );
    assert!(
        resp.agent_key_pem.contains("BEGIN PRIVATE KEY"),
        "agent key PEM expected"
    );
    assert!(
        resp.ca_root_pem.contains("BEGIN CERTIFICATE"),
        "CA root PEM expected"
    );
    assert_eq!(resp.heartbeat_interval_secs, 10);
}

#[tokio::test]
async fn enroll_with_invalid_enrollment_token_returns_unauthenticated() {
    let h = harness().await;
    let channel = channel_to(h.addr).await;
    let bearer: MetadataValue<_> = format!("Bearer {TEST_TOKEN}").parse().unwrap();
    let mut client = ControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });

    let err = client
        .enroll(sample_request(
            "DOESNOTEXISTXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        ))
        .await
        .expect_err("must fail with invalid enrollment token");
    assert_eq!(err.code(), Code::Unauthenticated, "got {err:?}");
}
