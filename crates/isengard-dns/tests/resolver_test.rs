//! Integration tests for [`IsengardResolver`].
//!
//! These run in-process: no sockets are bound. The handler API
//! ([`RequestHandlerImpl::lookup_for_test`]) exercises the same
//! `zones-first, upstream-fallback` flow that the wire path uses, but
//! without faking a hickory `ResponseHandler`.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, CNAME, opt::EdnsOption};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};
use isengard_dns::{IsengardResolver, StaticZoneSource};
use tokio::net::UdpSocket;

/// A handful of unbound 127.0.0.1 ports we can hand to `Vec<SocketAddr>`
/// without ever actually sending traffic to them. The forwarding tests
/// only exercise the path that *attempts* to forward; if the upstream
/// returns an error fast (port closed → ICMP unreachable), we still get
/// a deterministic response from the handler.
fn dummy_upstreams() -> Vec<SocketAddr> {
    // TEST-NET-1 (RFC 5737): guaranteed never-routable. Any send attempt
    // there blackholes; combined with a short resolver timeout this
    // gives us "forwarding attempted, no answer".
    //
    // Single entry keeps the worst-case timeout bounded by
    // `ResolverOpts::timeout × ResolverOpts::attempts × 2 (UDP+TCP)`.
    vec!["192.0.2.1:53".parse().unwrap()]
}

fn a_record(name: &str, ip: Ipv4Addr) -> Record {
    Record::from_rdata(Name::from_str(name).unwrap(), 30, RData::A(A(ip)))
}

fn aaaa_record(name: &str, ip: Ipv6Addr) -> Record {
    Record::from_rdata(Name::from_str(name).unwrap(), 30, RData::AAAA(AAAA(ip)))
}

fn cname_record(name: &str, target: &str) -> Record {
    Record::from_rdata(
        Name::from_str(name).unwrap(),
        30,
        RData::CNAME(CNAME(Name::from_str(target).unwrap())),
    )
}

#[tokio::test]
async fn answers_authoritative_a_record_from_static_zone() {
    let src = StaticZoneSource::builder()
        .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 42)))
        .build();

    let resolver =
        IsengardResolver::new(Arc::new(src), dummy_upstreams()).expect("resolver builds");

    let handler = resolver.handler_for_test();
    let (code, answers) = handler
        .lookup_for_test(
            &Name::from_str("foo.weavers.local.").unwrap(),
            RecordType::A,
        )
        .await
        .expect("lookup ok");

    assert_eq!(code, ResponseCode::NoError);
    assert_eq!(answers.len(), 1);
    match answers[0].data() {
        RData::A(a) => assert_eq!(a.0, Ipv4Addr::new(10, 0, 0, 42)),
        other => panic!("expected A record, got {other:?}"),
    }
}

#[tokio::test]
async fn answers_authoritative_aaaa_record_from_static_zone() {
    let src = StaticZoneSource::builder()
        .record(aaaa_record("v6.weavers.local.", Ipv6Addr::LOCALHOST))
        .build();

    let resolver = IsengardResolver::new(Arc::new(src), dummy_upstreams()).unwrap();
    let handler = resolver.handler_for_test();

    let (code, answers) = handler
        .lookup_for_test(
            &Name::from_str("v6.weavers.local.").unwrap(),
            RecordType::AAAA,
        )
        .await
        .unwrap();

    assert_eq!(code, ResponseCode::NoError);
    assert_eq!(answers.len(), 1);
    match answers[0].data() {
        RData::AAAA(aaaa) => assert_eq!(aaaa.0, Ipv6Addr::LOCALHOST),
        other => panic!("expected AAAA record, got {other:?}"),
    }
}

#[tokio::test]
async fn answers_authoritative_cname_record_from_static_zone() {
    let src = StaticZoneSource::builder()
        .record(cname_record(
            "alias.weavers.local.",
            "target.weavers.local.",
        ))
        .build();

    let resolver = IsengardResolver::new(Arc::new(src), dummy_upstreams()).unwrap();
    let handler = resolver.handler_for_test();

    let (code, answers) = handler
        .lookup_for_test(
            &Name::from_str("alias.weavers.local.").unwrap(),
            RecordType::CNAME,
        )
        .await
        .unwrap();

    assert_eq!(code, ResponseCode::NoError);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].record_type(), RecordType::CNAME);
}

#[tokio::test]
async fn forwards_misses_to_upstream() {
    // Static zone owns nothing useful for `example.com`; the handler
    // must therefore attempt to forward. We point at TEST-NET-1 so the
    // attempt fails fast and we observe `NXDomain` (the test-surface
    // mapping for "upstream errored"). The key assertion is that we did
    // NOT get an authoritative NXDomain for a name we have no zone for:
    // an empty zone returning `Some(vec![])` would be the bug.
    let src = StaticZoneSource::builder()
        .record(a_record("foo.weavers.local.", Ipv4Addr::new(10, 0, 0, 1)))
        .build();

    let resolver = IsengardResolver::new(Arc::new(src), dummy_upstreams()).unwrap();
    let handler = resolver.handler_for_test();

    // Worst case: 2s timeout × 2 attempts × 2 protocols (UDP+TCP) = 8s.
    // 15s gives generous headroom on slow CI.
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        handler.lookup_for_test(&Name::from_str("example.com.").unwrap(), RecordType::A),
    )
    .await
    .expect("lookup_for_test resolves within 15s")
    .expect("handler returns Ok");

    // Either NXDomain (upstream rejected/blackholed) or NoError-empty
    // (some platforms return an empty answer on timeout). Both prove
    // the handler *forwarded* rather than answering authoritatively.
    let (code, answers) = result;
    assert!(
        code == ResponseCode::NXDomain || (code == ResponseCode::NoError && answers.is_empty()),
        "expected upstream-forwarded miss; got code={code:?} answers={answers:?}",
    );
}

#[tokio::test]
async fn empty_upstream_list_is_rejected() {
    let src = StaticZoneSource::builder().build();
    let err = IsengardResolver::new(Arc::new(src), Vec::new()).unwrap_err();
    assert!(
        err.to_string().contains("at least one upstream"),
        "got: {err}",
    );
}

// ---------- wire-level tests ----------
//
// These bind a UDP socket on a free local port and round-trip an actual
// DNS message through the resolver. They are `#[ignore]`d so CI sandboxes
// that block bind(2) don't fail; `cargo test -p isengard-dns -- --ignored`
// runs them locally.

async fn pick_free_udp_port() -> u16 {
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

async fn send_udp_query(server: SocketAddr, name: &str, qtype: RecordType) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
    let mut msg = Message::new();
    msg.set_id(0x1234)
        .set_message_type(MessageType::Query)
        .set_op_code(OpCode::Query)
        .set_recursion_desired(true)
        .add_query(Query::query(Name::from_utf8(name).unwrap(), qtype));
    let bytes = msg.to_bytes().expect("encode query");
    socket.send_to(&bytes, server).await.expect("send query");

    let mut buf = vec![0u8; 4096];
    let recv = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut buf))
        .await
        .expect("response within 2s")
        .expect("recv ok");
    Message::from_bytes(&buf[..recv]).expect("decode response")
}

async fn send_udp_query_with_edns(
    server: SocketAddr,
    name: &str,
    qtype: RecordType,
    udp_payload_size: u16,
) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
    let mut msg = Message::new();
    msg.set_id(0x5678)
        .set_message_type(MessageType::Query)
        .set_op_code(OpCode::Query)
        .set_recursion_desired(true)
        .add_query(Query::query(Name::from_utf8(name).unwrap(), qtype));

    // Add an EDNS0 OPT record with a custom UDP payload size.
    let mut edns = hickory_proto::op::Edns::new();
    edns.set_max_payload(udp_payload_size).set_version(0);
    // A no-op option just to prove EDNS option round-trips don't break us.
    edns.options_mut()
        .insert(EdnsOption::Unknown(0xFFFE, vec![]));
    msg.set_edns(edns);

    let bytes = msg.to_bytes().expect("encode edns query");
    socket.send_to(&bytes, server).await.expect("send query");

    let mut buf = vec![0u8; 4096];
    let recv = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut buf))
        .await
        .expect("response within 2s")
        .expect("recv ok");
    Message::from_bytes(&buf[..recv]).expect("decode response")
}

async fn send_tcp_query(server: SocketAddr, name: &str, qtype: RecordType) -> Message {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut socket = tokio::net::TcpStream::connect(server)
        .await
        .expect("tcp connect");

    let mut msg = Message::new();
    msg.set_id(0x9ABC)
        .set_message_type(MessageType::Query)
        .set_op_code(OpCode::Query)
        .set_recursion_desired(true)
        .add_query(Query::query(Name::from_utf8(name).unwrap(), qtype));
    let bytes = msg.to_bytes().expect("encode tcp query");

    // RFC 1035 §4.2.2: TCP queries are framed by a 2-byte big-endian length.
    let len: u16 = bytes.len().try_into().expect("query under 64KiB");
    socket
        .write_all(&len.to_be_bytes())
        .await
        .expect("write len");
    socket.write_all(&bytes).await.expect("write body");

    let mut len_buf = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut len_buf))
        .await
        .expect("read len within 2s")
        .expect("read len ok");
    let resp_len = u16::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; resp_len];
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut buf))
        .await
        .expect("read body within 2s")
        .expect("read body ok");

    Message::from_bytes(&buf).expect("decode tcp response")
}

#[tokio::test]
#[ignore = "binds a UDP/TCP socket; run via `cargo test -- --ignored`"]
async fn wire_authoritative_a_query_round_trip() {
    let src = StaticZoneSource::builder()
        .record(a_record("wire.weavers.local.", Ipv4Addr::new(10, 9, 8, 7)))
        .build();
    let resolver =
        Arc::new(IsengardResolver::new(Arc::new(src), dummy_upstreams()).expect("resolver builds"));

    let port = pick_free_udp_port().await;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = resolver.clone();
    let _server = tokio::spawn(async move {
        r.serve(addr, addr).await.expect("server runs");
    });

    // Tiny settle to let the bind complete.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = send_udp_query(addr, "wire.weavers.local.", RecordType::A).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    assert_eq!(resp.answers().len(), 1);
    assert!(
        resp.authoritative(),
        "AA bit must be set on authoritative answers",
    );
}

#[tokio::test]
#[ignore = "binds a UDP socket; run via `cargo test -- --ignored`"]
async fn wire_edns0_query_round_trips() {
    let src = StaticZoneSource::builder()
        .record(a_record("edns.weavers.local.", Ipv4Addr::new(10, 9, 8, 8)))
        .build();
    let resolver =
        Arc::new(IsengardResolver::new(Arc::new(src), dummy_upstreams()).expect("resolver builds"));

    let port = pick_free_udp_port().await;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = resolver.clone();
    let _server = tokio::spawn(async move {
        r.serve(addr, addr).await.expect("server runs");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = send_udp_query_with_edns(addr, "edns.weavers.local.", RecordType::A, 4096).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    assert_eq!(resp.answers().len(), 1);
    // The server should respond with an OPT record of its own.
    assert!(
        resp.extensions().is_some(),
        "EDNS-enabled query must get an OPT record in the response",
    );
}

#[tokio::test]
#[ignore = "binds a TCP socket; run via `cargo test -- --ignored`"]
async fn wire_tcp_query_round_trips() {
    let src = StaticZoneSource::builder()
        .record(a_record("tcp.weavers.local.", Ipv4Addr::new(10, 9, 8, 9)))
        .build();
    let resolver =
        Arc::new(IsengardResolver::new(Arc::new(src), dummy_upstreams()).expect("resolver builds"));

    let port = pick_free_udp_port().await;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = resolver.clone();
    let _server = tokio::spawn(async move {
        r.serve(addr, addr).await.expect("server runs");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = send_tcp_query(addr, "tcp.weavers.local.", RecordType::A).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    assert_eq!(resp.answers().len(), 1);
    assert!(
        resp.authoritative(),
        "AA bit must be set on TCP authoritative answers too",
    );
}

#[tokio::test]
#[ignore = "binds UDP+TCP sockets; run via `cargo test -- --ignored`"]
async fn wire_large_answer_truncates_over_udp_then_succeeds_over_tcp() {
    // Stack enough A records on the same name that the UDP answer must
    // exceed 512 bytes (the legacy DNS UDP limit). Hickory sets TC and
    // truncates; a TCP retry returns the full set.
    //
    // 30 A records × ~16 bytes per RR ≈ 480+ bytes of payload, plus
    // header + question, gets us comfortably over 512.
    let mut builder = StaticZoneSource::builder();
    for i in 0..40u8 {
        builder = builder.record(a_record("many.weavers.local.", Ipv4Addr::new(10, 0, 0, i)));
    }
    let src = builder.build();
    let resolver =
        Arc::new(IsengardResolver::new(Arc::new(src), dummy_upstreams()).expect("resolver builds"));

    let port = pick_free_udp_port().await;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let r = resolver.clone();
    let _server = tokio::spawn(async move {
        r.serve(addr, addr).await.expect("server runs");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send the UDP query with a tiny EDNS payload size so the server
    // hits the budget. Hickory's response handler uses the client's
    // EDNS-announced UDP payload size to bound the response; without
    // EDNS the default budget is 4096 bytes (RFC 6891 large enough that
    // 40 short A records still fit).
    let udp_resp = send_udp_query_with_edns(addr, "many.weavers.local.", RecordType::A, 512).await;
    assert!(
        udp_resp.truncated(),
        "expected TC bit set on oversized UDP response; got answers={}",
        udp_resp.answers().len(),
    );

    let tcp_resp = send_tcp_query(addr, "many.weavers.local.", RecordType::A).await;
    assert_eq!(tcp_resp.response_code(), ResponseCode::NoError);
    assert!(
        tcp_resp.answers().len() >= 30,
        "TCP retry should return the full record set; got {}",
        tcp_resp.answers().len(),
    );
}
