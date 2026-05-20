//! End-to-end diagnostic surface check. Drives the real `LspService`
//! through `initialize` + `did_open` and asserts the diagnostics the
//! server publishes against a compose document with several bad labels.

use futures::StreamExt;
use serde_json::{Value, json};
use tower::{Service, ServiceExt};
use tower_lsp::LspService;
use tower_lsp::lsp_types::Url;

const BAD_COMPOSE: &str = r#"services:
  web:
    image: nginx
    labels:
      isengard.enable: "true"
      isengard.policy.gate: maybe
      isengard.expose.port: "70000"
      isengard.hooks.post_deploy: not-a-url
      isengard.unknown: oops
"#;

/// Build a JSON-RPC `Request` from a literal payload. Hides the
/// serde_json round trip that tower-lsp's `jsonrpc::Request` requires.
fn rpc(payload: Value) -> tower_lsp::jsonrpc::Request {
    serde_json::from_value(payload).unwrap()
}

#[tokio::test]
async fn publishes_diagnostics_for_bad_compose() {
    let (service, socket) = LspService::new(isengard_lsp::test_backend);

    // Drive every server call from a spawned task so this thread can
    // drain the outbound notification stream concurrently. tower-lsp's
    // client channel pushes `publishDiagnostics` through the same socket
    // we read here; if the driver and the reader are on the same task,
    // the channel back-pressures and the test hangs.
    let driver = tokio::spawn(async move {
        let mut service = service;
        // initialize.
        let _ = service
            .ready()
            .await
            .unwrap()
            .call(rpc(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "capabilities": {} }
            })))
            .await
            .unwrap();
        // initialized.
        let _ = service
            .ready()
            .await
            .unwrap()
            .call(rpc(json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {}
            })))
            .await
            .unwrap();
        // didOpen the bad compose. URI basename ends in `compose.yaml` so
        // the server treats it as `DocKind::Compose`.
        let _ = service
            .ready()
            .await
            .unwrap()
            .call(rpc(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": "file:///workspace/compose.yaml",
                        "languageId": "yaml",
                        "version": 1,
                        "text": BAD_COMPOSE,
                    }
                }
            })))
            .await
            .unwrap();
    });

    // Drain notifications until we see a publishDiagnostics for our URI,
    // or hit a short timeout. The driver task races us through the LSP
    // surface and emits the notification asynchronously after didOpen.
    let target = Url::parse("file:///workspace/compose.yaml").unwrap();
    let publish = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut socket = socket;
        while let Some(msg) = socket.next().await {
            let value = serde_json::to_value(&msg).unwrap();
            if value["method"] == "textDocument/publishDiagnostics"
                && value["params"]["uri"] == json!(target.to_string())
            {
                return value;
            }
        }
        panic!("socket closed before publishDiagnostics arrived");
    })
    .await
    .expect("publishDiagnostics did not arrive within 5s");

    driver.await.unwrap();

    let diags = publish["params"]["diagnostics"].as_array().expect("array");

    // Four bad entries: unknown key, bad enum, out-of-range port, bad URL.
    // The clean `isengard.enable: "true"` row must not show.
    assert_eq!(
        diags.len(),
        4,
        "expected 4 diagnostics, got: {}",
        serde_json::to_string_pretty(diags).unwrap()
    );
    for d in diags {
        assert_eq!(d["source"], json!("isengard"));
        assert_eq!(d["severity"], json!(1)); // ERROR
    }
    let messages: Vec<&str> = diags
        .iter()
        .map(|d| d["message"].as_str().unwrap())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("unknown isengard label")),
        "expected unknown-label diag, got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("expected one of")),
        "expected enum diag, got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("port in 1..=65535")),
        "expected port diag, got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("URL")),
        "expected URL diag, got {messages:?}"
    );
}
