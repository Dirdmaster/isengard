//! Phase 2a integration test: spin up the controller library on an ephemeral
//! port, connect a tonic client, assert both RPCs return Unimplemented.

use std::net::SocketAddr;
use std::time::Duration;

use isengard_controller::{ControllerOptions, run_controller};
use isengard_proto::pb::EnrollRequest;
use isengard_proto::pb::controller_client::ControllerClient;
use tonic::Code;

async fn spawn_controller_on_ephemeral_port() -> SocketAddr {
    // Pick an ephemeral port by binding 0 first, then release it so tonic
    // can rebind. Race-prone in theory, fine in practice for tests.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    tokio::spawn(async move {
        let _ = run_controller(ControllerOptions {
            listen: addr,
            state_dir: std::env::temp_dir(),
            config: serde_json::Value::Object(Default::default()),
        })
        .await;
    });

    // Wait for the server to bind; poll up to 2s.
    for _ in 0..40 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("controller did not bind {addr} within 2s");
}

#[tokio::test]
async fn enroll_returns_unimplemented() {
    let addr = spawn_controller_on_ephemeral_port().await;
    let mut client = ControllerClient::connect(format!("http://{addr}"))
        .await
        .expect("connect");

    let err = client
        .enroll(EnrollRequest::default())
        .await
        .expect_err("Enroll should return an error in Phase 2a");

    assert_eq!(err.code(), Code::Unimplemented, "got: {err:?}");
}

#[tokio::test]
async fn server_accepts_concurrent_connections() {
    let addr = spawn_controller_on_ephemeral_port().await;

    // Open three clients in parallel; all should connect and receive
    // Unimplemented from Enroll.
    let mut handles = Vec::new();
    for _ in 0..3 {
        handles.push(tokio::spawn(async move {
            let mut client = ControllerClient::connect(format!("http://{addr}"))
                .await
                .expect("connect");
            let err = client.enroll(EnrollRequest::default()).await.unwrap_err();
            assert_eq!(err.code(), Code::Unimplemented);
        }));
    }
    for h in handles {
        h.await.expect("task");
    }
}
