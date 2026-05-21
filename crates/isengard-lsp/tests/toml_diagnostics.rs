//! Open a broken `stack.toml` document against the LSP service and
//! assert that `textDocument/publishDiagnostics` reports the
//! expected error. Locks the wire surface so a future refactor of
//! the diagnostics module can't silently regress the editor side.
//!
//! Implementation note: tower-lsp's `LspService::new` returns a
//! `(service, messages)` pair where `messages` is a bounded mpsc
//! stream the server pushes notifications into. A handler that does
//! `client.publish_diagnostics(...).await` blocks on that channel.
//!
//! The earlier version of these tests called the service's
//! `did_open` method directly on the test thread and then read the
//! messages channel after the call returned. That deadlocks: the
//! handler cannot return until it has pushed the notification, but
//! the test thread is the only thing draining the channel. CI
//! nextest would sit there for hours.
//!
//! Fix: drive the service from a spawned task. The test side becomes
//! a pure client that sends requests + drains the message stream
//! concurrently.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tower::Service;
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;

/// Spawns the LSP service on its own task and returns:
/// - a sender for client-to-server requests / notifications
/// - a receiver of server-to-client messages (notifications + responses)
/// - the JoinHandle so the test can await shutdown (unused; the task
///   exits when both channels close).
///
/// All channel-blocking happens off the test thread, so a handler
/// that pushes a `publishDiagnostics` notification cannot deadlock
/// against the test reader.
fn spawn_service() -> (
    mpsc::Sender<(Request, mpsc::Sender<Option<Value>>)>,
    mpsc::Receiver<Value>,
    JoinHandle<()>,
) {
    let (req_tx, mut req_rx) = mpsc::channel::<(Request, mpsc::Sender<Option<Value>>)>(32);
    let (msg_tx, msg_rx) = mpsc::channel::<Value>(64);

    let handle = tokio::spawn(async move {
        let (service, mut messages) = LspService::new(isengard_lsp::test_backend);
        let mut service = service;

        loop {
            tokio::select! {
                maybe_req = req_rx.recv() => {
                    match maybe_req {
                        Some((req, reply_tx)) => {
                            let reply = match futures_util::future::poll_fn(|cx| service.poll_ready(cx)).await {
                                Ok(()) => match service.call(req).await {
                                    Ok(r) => r.map(|r| serde_json::to_value(&r).unwrap()),
                                    Err(_) => None,
                                },
                                Err(_) => None,
                            };
                            let _ = reply_tx.send(reply).await;
                        }
                        None => break,
                    }
                }
                maybe_msg = messages.next() => {
                    match maybe_msg {
                        Some(msg) => {
                            let v = serde_json::to_value(&msg).unwrap();
                            if msg_tx.send(v).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    (req_tx, msg_rx, handle)
}

async fn call(
    req_tx: &mpsc::Sender<(Request, mpsc::Sender<Option<Value>>)>,
    payload: Value,
) -> Option<Value> {
    let request: Request = serde_json::from_value(payload).unwrap();
    let (reply_tx, mut reply_rx) = mpsc::channel(1);
    req_tx.send((request, reply_tx)).await.unwrap();
    reply_rx.recv().await.unwrap()
}

async fn notify(
    req_tx: &mpsc::Sender<(Request, mpsc::Sender<Option<Value>>)>,
    payload: Value,
) {
    let _ = call(req_tx, payload).await;
}

/// Wait up to `budget` for the first message satisfying `pred`. Drops
/// every non-matching message on the floor. Returns `None` on
/// timeout.
async fn wait_for<F>(
    rx: &mut mpsc::Receiver<Value>,
    budget: Duration,
    mut pred: F,
) -> Option<Value>
where
    F: FnMut(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => {
                if pred(&msg) {
                    return Some(msg);
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn missing_name_in_stack_toml_publishes_diagnostic() {
    let (req_tx, mut messages, _handle) = spawn_service();

    call(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    )
    .await;

    notify(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await;

    let uri = "file:///workspace/hello/stack.toml";
    notify(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "toml",
                    "version": 1,
                    "text": "compose = [\"compose.yaml\"]\n"
                }
            }
        }),
    )
    .await;

    let v = wait_for(&mut messages, Duration::from_secs(3), |v| {
        v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == uri
    })
    .await
    .expect("expected a publishDiagnostics notification within 3s");

    let diags = v["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1);
    assert!(
        diags[0]["message"].as_str().unwrap().contains("name"),
        "diag message: {}",
        diags[0]["message"]
    );
    assert_eq!(diags[0]["severity"], json!(1)); // Error
    assert_eq!(diags[0]["source"], json!("isengard"));
}

#[tokio::test]
async fn valid_stack_toml_publishes_empty_diagnostics() {
    let (req_tx, mut messages, _handle) = spawn_service();

    call(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    )
    .await;

    notify(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await;

    let uri = "file:///workspace/ok/stack.toml";
    notify(
        &req_tx,
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "toml",
                    "version": 1,
                    "text": "name = \"ok\"\ncompose = [\"compose.yaml\"]\n"
                }
            }
        }),
    )
    .await;

    let v = wait_for(&mut messages, Duration::from_secs(3), |v| {
        v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == uri
    })
    .await
    .expect("expected a publishDiagnostics notification within 3s");

    let diags = v["params"]["diagnostics"].as_array().unwrap();
    assert!(diags.is_empty(), "expected no diags, got {diags:?}");
}
