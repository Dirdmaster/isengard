//! v0.3b end-to-end test for the controller-hosted DNS resolver.
//!
//! Boots the resolver on a high port, populates a fixture routing rule plus
//! a host LAN IP, and asserts:
//!
//! - In-zone known names return the agent's IP (NoError + A record).
//! - In-zone unknown names return NXDOMAIN.
//! - Out-of-zone names return REFUSED (controller does not recurse).
//!
//! The test uses raw UDP + `hickory-proto` to encode/decode messages; that's
//! lighter than spinning up a full `hickory-resolver` client and avoids
//! dependence on the resolver's caching/retry logic.
//!
//! Marked `#[ignore]` -> run on demand. CI sandboxes that block UDP bind
//! would otherwise cause spurious failures; local `cargo test --workspace
//! -- --ignored` exercises it.

use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};
use isengard_controller::dns::{DnsResolver, RECORD_TTL_SECS};
use isengard_storage::{
    EnrollHost, InsertRoutingRule, Inventory, RoutingRuleSource, RoutingRuleState, TlsMode,
};
use tempfile::tempdir;
use tokio::net::UdpSocket;

async fn send_query(server: std::net::SocketAddr, name: &str, qtype: RecordType) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
    let mut msg = Message::new();
    msg.set_id(0xBEEF)
        .set_message_type(MessageType::Query)
        .set_op_code(OpCode::Query)
        .set_recursion_desired(true)
        .add_query(Query::query(Name::from_utf8(name).unwrap(), qtype));
    let bytes = msg.to_bytes().expect("encode query");
    socket.send_to(&bytes, server).await.expect("send query");

    let mut buf = vec![0u8; 1500];
    let recv = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut buf))
        .await
        .expect("response within 2s")
        .expect("recv");
    Message::from_bytes(&buf[..recv]).expect("decode response")
}

async fn fixture_with_rule(
    zone: &str,
    hostname: &str,
    lan_ip: &str,
) -> (Arc<Inventory>, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let inv = Arc::new(
        Inventory::open(&dir.path().join("isengard.db"))
            .await
            .unwrap(),
    );

    let host = inv
        .enroll_host(EnrollHost {
            fingerprint: format!("fp-{hostname}"),
            hostname: format!("agent-{hostname}"),
            os: "linux".into(),
            arch: "x86_64".into(),
            agent_version: "0".into(),
            docker_version: "27".into(),
        })
        .await
        .unwrap();

    inv.set_host_lan_ip(host, lan_ip).await.unwrap();

    inv.insert_routing_rule(InsertRoutingRule {
        host_id: host,
        stack_id: None,
        service_name: "svc".into(),
        container_port: 80,
        public_hostname: format!("{hostname}.{zone}"),
        protocol: "http".into(),
        adapter: "none".into(),
        tls_mode: TlsMode::Edge,
        healthcheck_path: None,
        healthcheck_interval_secs: 10,
        auth: None,
        state: RoutingRuleState::Pending,
        source: RoutingRuleSource::Ui,
        source_container_id: None,
        source_imported_from: None,
    })
    .await
    .unwrap();

    (inv, dir)
}

/// Pick a high UDP port the resolver can bind to. We bind a probe socket,
/// read its port, drop it, and hand the port to the resolver. There's a
/// small race window between drop and rebind but it's fine for a one-shot
/// test invocation.
async fn pick_free_udp_port() -> u16 {
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

#[tokio::test]
#[ignore = "binds a UDP socket; run via `cargo test -- --ignored` or locally"]
async fn resolver_answers_in_zone_query_with_lan_ip() {
    let (inv, _dir) = fixture_with_rule("iso", "hello", "192.168.1.42").await;

    let port = pick_free_udp_port().await;
    let listen: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let resolver = DnsResolver::new("iso".into(), inv);
    let _handle = resolver.start(listen).await.expect("resolver start");

    let resp = send_query(listen, "hello.iso", RecordType::A).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    assert_eq!(resp.answers().len(), 1, "exactly one A answer expected");
    let answer = &resp.answers()[0];
    assert_eq!(answer.ttl(), RECORD_TTL_SECS);
    match answer.data() {
        RData::A(a) => assert_eq!(a.0.to_string(), "192.168.1.42"),
        other => panic!("unexpected rdata type: {other:?}"),
    }
}

#[tokio::test]
#[ignore = "binds a UDP socket; run via `cargo test -- --ignored` or locally"]
async fn resolver_returns_nxdomain_for_unknown_in_zone_name() {
    let (inv, _dir) = fixture_with_rule("iso", "hello", "192.168.1.42").await;

    let port = pick_free_udp_port().await;
    let listen: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let resolver = DnsResolver::new("iso".into(), inv);
    let _handle = resolver.start(listen).await.expect("resolver start");

    let resp = send_query(listen, "ghost.iso", RecordType::A).await;
    assert_eq!(resp.response_code(), ResponseCode::NXDomain);
    assert!(resp.answers().is_empty());
}

#[tokio::test]
#[ignore = "binds a UDP socket; run via `cargo test -- --ignored` or locally"]
async fn resolver_refuses_out_of_zone_query() {
    let (inv, _dir) = fixture_with_rule("iso", "hello", "192.168.1.42").await;

    let port = pick_free_udp_port().await;
    let listen: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let resolver = DnsResolver::new("iso".into(), inv);
    let _handle = resolver.start(listen).await.expect("resolver start");

    let resp = send_query(listen, "google.com", RecordType::A).await;
    assert_eq!(
        resp.response_code(),
        ResponseCode::Refused,
        "controller does not recurse; outside-zone -> REFUSED so the OS \
         falls through to its upstream resolver",
    );
}
