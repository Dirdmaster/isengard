//! Phase 13B: agent-side log tailer.
//!
//! On a `ControllerMessage::StartLogStream`, the agent opens a bollard
//! `containers.logs(LogsOptions { follow:true, stdout:true, stderr:true,
//! timestamps:true, tail })` stream, decodes each frame, splits on `\n`, and
//! emits one `AgentMessage::LogChunk` per line back through the existing Sync
//! stream. The `subscription_id` lets the controller demux concurrent
//! subscriptions from the same agent (e.g. multiple WebSocket clients on the
//! dashboard).
//!
//! The bollard interaction is hidden behind the [`LogSource`] trait so unit
//! tests drive the tailer with synthetic line streams without needing a live
//! Docker daemon.
//!
//! Backpressure: the tailer uses `mpsc::Sender::try_send`. On a full buffer
//! we increment a per-subscription drop counter and emit a `KIND_DROPPED`
//! frame ahead of the next successful `KIND_LINE`. We never block.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use isengard_proto::pb::{AgentMessage, LogChunk, agent_message, log_chunk};
use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;
use tracing::{debug, warn};

/// One decoded log line ready to be wrapped in a `LogChunk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedLine {
    /// "stdout" or "stderr".
    pub stream: &'static str,
    /// RFC3339 timestamp the daemon stamped on the frame.
    pub occurred_at: String,
    /// The line body, trailing newline stripped, lossy UTF-8.
    pub line: String,
}

/// Pluggable log producer: bollard in production, fakes in tests.
#[async_trait]
pub trait LogSource: Send + Sync + 'static {
    /// Open a tail on `container_name`. The returned stream emits decoded
    /// lines until the source ends (container gone, follow=false drained) or
    /// errors (which we surface as Unavailable).
    async fn tail(
        &self,
        container_name: &str,
        tail: u32,
        follow: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DecodedLine, String>> + Send>>, String>;
}

/// Production [`LogSource`]: wraps a `bollard::Docker` handle.
pub struct BollardLogSource {
    pub docker: Arc<bollard::Docker>,
}

#[async_trait]
impl LogSource for BollardLogSource {
    async fn tail(
        &self,
        container_name: &str,
        tail: u32,
        follow: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DecodedLine, String>> + Send>>, String> {
        use bollard::container::LogsOptions;
        let opts = LogsOptions::<String> {
            follow,
            stdout: true,
            stderr: true,
            timestamps: true,
            tail: tail.to_string(),
            ..Default::default()
        };
        let raw = self.docker.logs(container_name, Some(opts));
        // Map bollard frames to DecodedLine.
        let mapped = raw.map(|r| match r {
            Ok(out) => Ok(decode_bollard_frame(out)),
            Err(e) => Err(format!("{e}")),
        });
        // bollard yields one DecodedLine per frame; split multi-line bodies
        // upstream (decode_bollard_frame already handles `\n` by joining with
        // a stable separator the runner re-splits).
        Ok(Box::pin(mapped))
    }
}

/// Decode one `bollard::container::LogOutput` frame into a `DecodedLine`. If
/// the frame body contains internal newlines, they are preserved verbatim;
/// the [`run_tail`] consumer splits them out into separate chunks.
fn decode_bollard_frame(out: bollard::container::LogOutput) -> DecodedLine {
    let (stream, bytes) = match out {
        bollard::container::LogOutput::StdOut { message } => ("stdout", message),
        bollard::container::LogOutput::StdErr { message } => ("stderr", message),
        bollard::container::LogOutput::StdIn { message } => ("stdout", message),
        bollard::container::LogOutput::Console { message } => ("stdout", message),
    };
    let body = String::from_utf8_lossy(&bytes).into_owned();
    // With timestamps=true, the daemon prefixes each line with an RFC3339Nano
    // timestamp followed by a space. Strip + capture it.
    let (ts, rest) = match body.split_once(' ') {
        Some((ts, rest)) if ts.len() >= 20 && ts.contains('T') => {
            (ts.to_string(), rest.to_string())
        }
        _ => (String::new(), body),
    };
    DecodedLine {
        stream,
        occurred_at: ts,
        line: rest.trim_end_matches('\n').to_string(),
    }
}

/// Drive a [`LogSource`] tail to completion, emitting `LogChunk` frames into
/// `out`. Honors `abort` by exiting on `true`.
///
/// This function is the unit of work each `StartLogStream` spawns.
pub async fn run_tail<S: LogSource>(
    source: Arc<S>,
    subscription_id: String,
    container_name: String,
    tail: u32,
    follow: bool,
    mut abort: watch::Receiver<bool>,
    out: mpsc::Sender<AgentMessage>,
) {
    let mut stream = match source.tail(&container_name, tail, follow).await {
        Ok(s) => s,
        Err(e) => {
            send_unavailable(&out, &subscription_id, &e).await;
            return;
        }
    };

    let mut dropped: u32 = 0;
    let mut backfill_remaining = tail;

    loop {
        tokio::select! {
            biased;
            changed = abort.changed() => {
                if changed.is_ok() && *abort.borrow() {
                    debug!(sub = %subscription_id, "log tail aborted");
                    return;
                }
            }
            next = stream.next() => match next {
                Some(Ok(decoded)) => {
                    // Multi-line bodies: split and emit a chunk per line.
                    for piece in decoded.line.split('\n') {
                        if piece.is_empty() {
                            continue;
                        }
                        let kind = if backfill_remaining > 0 {
                            backfill_remaining -= 1;
                            log_chunk::Kind::Backfill
                        } else {
                            log_chunk::Kind::Line
                        };
                        // Flush an accumulated drop count BEFORE the next line.
                        if dropped > 0
                            && try_send_chunk(&out, drop_chunk(&subscription_id, dropped))
                        {
                            dropped = 0;
                        }
                        let chunk = LogChunk {
                            subscription_id: subscription_id.clone(),
                            kind: kind as i32,
                            occurred_at: decoded.occurred_at.clone(),
                            stream: decoded.stream.to_string(),
                            line: piece.to_string(),
                            dropped: 0,
                            reason: String::new(),
                        };
                        if !try_send_chunk(&out, chunk) {
                            dropped = dropped.saturating_add(1);
                        }
                    }
                }
                Some(Err(e)) => {
                    send_unavailable(&out, &subscription_id, &e).await;
                    return;
                }
                None => {
                    // Stream ended (container exited, follow=false drained).
                    debug!(sub = %subscription_id, "log tail ended");
                    return;
                }
            }
        }
    }
}

fn drop_chunk(sub: &str, count: u32) -> LogChunk {
    LogChunk {
        subscription_id: sub.to_string(),
        kind: log_chunk::Kind::Dropped as i32,
        occurred_at: String::new(),
        stream: String::new(),
        line: String::new(),
        dropped: count,
        reason: "backpressure".to_string(),
    }
}

/// Wrap the chunk in an `AgentMessage::LogChunk` and `try_send`. Returns
/// false on a full / closed channel.
fn try_send_chunk(out: &mpsc::Sender<AgentMessage>, chunk: LogChunk) -> bool {
    let msg = AgentMessage {
        payload: Some(agent_message::Payload::LogChunk(chunk)),
    };
    match out.try_send(msg) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => false,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!("log tail outbound closed; dropping chunk");
            false
        }
    }
}

async fn send_unavailable(out: &mpsc::Sender<AgentMessage>, sub: &str, reason: &str) {
    let chunk = LogChunk {
        subscription_id: sub.to_string(),
        kind: log_chunk::Kind::Unavailable as i32,
        occurred_at: String::new(),
        stream: String::new(),
        line: String::new(),
        dropped: 0,
        reason: reason.to_string(),
    };
    let msg = AgentMessage {
        payload: Some(agent_message::Payload::LogChunk(chunk)),
    };
    let _ = out.send(msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    /// Test-only LogSource: returns a canned vector of items.
    struct CannedSource {
        items: std::sync::Mutex<Option<Vec<Result<DecodedLine, String>>>>,
    }

    impl CannedSource {
        fn new(items: Vec<Result<DecodedLine, String>>) -> Self {
            Self {
                items: std::sync::Mutex::new(Some(items)),
            }
        }
    }

    #[async_trait]
    impl LogSource for CannedSource {
        async fn tail(
            &self,
            _container: &str,
            _tail: u32,
            _follow: bool,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DecodedLine, String>> + Send>>, String>
        {
            let items = self
                .items
                .lock()
                .unwrap()
                .take()
                .expect("CannedSource::tail called twice");
            Ok(Box::pin(stream::iter(items)))
        }
    }

    fn line(stream: &'static str, ts: &str, msg: &str) -> Result<DecodedLine, String> {
        Ok(DecodedLine {
            stream,
            occurred_at: ts.to_string(),
            line: msg.to_string(),
        })
    }

    fn collect_chunks(msgs: Vec<AgentMessage>) -> Vec<LogChunk> {
        msgs.into_iter()
            .filter_map(|m| match m.payload {
                Some(agent_message::Payload::LogChunk(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn run_tail_emits_backfill_then_lines() {
        let source = Arc::new(CannedSource::new(vec![
            line("stdout", "2026-05-06T14:32:01Z", "starting"),
            line("stdout", "2026-05-06T14:32:02Z", "listening"),
            line("stderr", "2026-05-06T14:32:03Z", "warn: slow"),
        ]));
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        let (_abort_tx, abort_rx) = watch::channel(false);

        run_tail(source, "sub-1".into(), "c".into(), 1, true, abort_rx, tx).await;

        let mut got = Vec::new();
        while let Ok(Some(m)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
        {
            got.push(m);
        }
        let chunks = collect_chunks(got);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].kind, log_chunk::Kind::Backfill as i32);
        assert_eq!(chunks[1].kind, log_chunk::Kind::Line as i32);
        assert_eq!(chunks[2].kind, log_chunk::Kind::Line as i32);
        assert_eq!(chunks[2].stream, "stderr");
        assert_eq!(chunks[0].line, "starting");
    }

    #[tokio::test]
    async fn run_tail_aborts_on_signal() {
        // Source that yields forever (until explicitly closed).
        struct Forever;
        #[async_trait]
        impl LogSource for Forever {
            async fn tail(
                &self,
                _c: &str,
                _t: u32,
                _f: bool,
            ) -> Result<Pin<Box<dyn Stream<Item = Result<DecodedLine, String>> + Send>>, String>
            {
                let s = stream::unfold(0, |n| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Some((line("stdout", "ts", &format!("n={n}")), n + 1))
                });
                Ok(Box::pin(s))
            }
        }

        let (tx, _rx) = mpsc::channel::<AgentMessage>(64);
        let (abort_tx, abort_rx) = watch::channel(false);

        let h = tokio::spawn(run_tail(
            Arc::new(Forever),
            "sub".into(),
            "c".into(),
            0,
            true,
            abort_rx,
            tx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        abort_tx.send(true).unwrap();
        // Should exit promptly.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), h)
            .await
            .expect("run_tail did not honor abort");
    }

    #[tokio::test]
    async fn run_tail_splits_multiline_bodies() {
        let multi = DecodedLine {
            stream: "stdout",
            occurred_at: "2026-05-06T14:32:01Z".into(),
            line: "first\nsecond\nthird".into(),
        };
        let source = Arc::new(CannedSource::new(vec![Ok(multi)]));
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        let (_a, abort) = watch::channel(false);

        run_tail(source, "sub".into(), "c".into(), 0, false, abort, tx).await;

        let mut got = Vec::new();
        while let Ok(Some(m)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
        {
            got.push(m);
        }
        let chunks = collect_chunks(got);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].line, "first");
        assert_eq!(chunks[1].line, "second");
        assert_eq!(chunks[2].line, "third");
    }

    #[tokio::test]
    async fn run_tail_emits_unavailable_on_error() {
        let source = Arc::new(CannedSource::new(vec![Err("container_not_found".into())]));
        let (tx, mut rx) = mpsc::channel::<AgentMessage>(8);
        let (_a, abort) = watch::channel(false);

        run_tail(source, "sub".into(), "c".into(), 0, true, abort, tx).await;

        let m = rx.recv().await.expect("expected at least one message");
        let chunks = collect_chunks(vec![m]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, log_chunk::Kind::Unavailable as i32);
        assert_eq!(chunks[0].reason, "container_not_found");
    }
}
