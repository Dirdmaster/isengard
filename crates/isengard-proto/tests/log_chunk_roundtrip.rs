//! Round-trip tests for the log-streaming wire types.

use isengard_proto::pb::{LogChunk, StartLogStream, StopLogStream, log_chunk};
use prost::Message;

#[test]
fn start_log_stream_round_trips() {
    let req = StartLogStream {
        subscription_id: "01HXXXXX0000000000000SUB01".into(),
        container_name: "blog_web_1".into(),
        tail: 200,
        follow: true,
    };
    let bytes = req.encode_to_vec();
    let back = StartLogStream::decode(&*bytes).unwrap();
    assert_eq!(back.subscription_id, req.subscription_id);
    assert_eq!(back.container_name, "blog_web_1");
    assert_eq!(back.tail, 200);
    assert!(back.follow);
}

#[test]
fn stop_log_stream_round_trips() {
    let req = StopLogStream {
        subscription_id: "01HXXXXX0000000000000SUB02".into(),
    };
    let bytes = req.encode_to_vec();
    let back = StopLogStream::decode(&*bytes).unwrap();
    assert_eq!(back.subscription_id, req.subscription_id);
}

#[test]
fn log_chunk_line_round_trips() {
    let chunk = LogChunk {
        subscription_id: "sub-1".into(),
        kind: log_chunk::Kind::Line as i32,
        occurred_at: "2026-05-06T14:32:01Z".into(),
        stream: "stdout".into(),
        line: "starting".into(),
        dropped: 0,
        reason: String::new(),
    };
    let bytes = chunk.encode_to_vec();
    let back = LogChunk::decode(&*bytes).unwrap();
    assert_eq!(back.subscription_id, "sub-1");
    assert_eq!(back.kind, log_chunk::Kind::Line as i32);
    assert_eq!(back.stream, "stdout");
    assert_eq!(back.line, "starting");
    assert_eq!(back.occurred_at, "2026-05-06T14:32:01Z");
}

#[test]
fn log_chunk_dropped_round_trips() {
    let chunk = LogChunk {
        subscription_id: "sub-1".into(),
        kind: log_chunk::Kind::Dropped as i32,
        occurred_at: String::new(),
        stream: String::new(),
        line: String::new(),
        dropped: 42,
        reason: "backpressure".into(),
    };
    let bytes = chunk.encode_to_vec();
    let back = LogChunk::decode(&*bytes).unwrap();
    assert_eq!(back.kind, log_chunk::Kind::Dropped as i32);
    assert_eq!(back.dropped, 42);
    assert_eq!(back.reason, "backpressure");
}

#[test]
fn log_chunk_unavailable_round_trips() {
    let chunk = LogChunk {
        subscription_id: "sub-1".into(),
        kind: log_chunk::Kind::Unavailable as i32,
        occurred_at: String::new(),
        stream: String::new(),
        line: String::new(),
        dropped: 0,
        reason: "container_not_found".into(),
    };
    let bytes = chunk.encode_to_vec();
    let back = LogChunk::decode(&*bytes).unwrap();
    assert_eq!(back.kind, log_chunk::Kind::Unavailable as i32);
    assert_eq!(back.reason, "container_not_found");
}
