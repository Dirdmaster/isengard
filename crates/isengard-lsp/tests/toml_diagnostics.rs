//! Open a broken `stack.toml` document against the LSP service and
//! assert that `textDocument/publishDiagnostics` reports the
//! expected error. Locks the wire surface so a future refactor of
//! the diagnostics module can't silently regress the editor side.

use serde_json::{Value, json};
use tower::{Service, ServiceExt};
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;

async fn call(
    service: &mut LspService<impl tower_lsp::LanguageServer>,
    payload: Value,
) -> Option<Value> {
    let request: Request = serde_json::from_value(payload).unwrap();
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response.map(|r| serde_json::to_value(&r).unwrap())
}

async fn notify(service: &mut LspService<impl tower_lsp::LanguageServer>, payload: Value) {
    let request: Request = serde_json::from_value(payload).unwrap();
    // Notifications have no `id`, tower-lsp returns Ok(None).
    let _ = service.ready().await.unwrap().call(request).await.unwrap();
}

#[tokio::test]
async fn missing_name_in_stack_toml_publishes_diagnostic() {
    let (service, mut messages) = LspService::new(isengard_lsp::test_backend);
    let mut service = service;

    call(
        &mut service,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    )
    .await;

    notify(
        &mut service,
        json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await;

    let uri = "file:///workspace/hello/stack.toml";
    notify(
        &mut service,
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

    // The server pushes a `publishDiagnostics` notification back to
    // the client. tower-lsp delivers these on the message stream
    // returned by `LspService::new`.
    let mut found = None;
    for _ in 0..16 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            futures_util::StreamExt::next(&mut messages),
        )
        .await
        {
            Ok(Some(msg)) => {
                let v = serde_json::to_value(&msg).unwrap();
                if v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == uri {
                    found = Some(v);
                    break;
                }
            }
            _ => break,
        }
    }

    let v = found.expect("expected a publishDiagnostics notification");
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
    let (service, mut messages) = LspService::new(isengard_lsp::test_backend);
    let mut service = service;

    call(
        &mut service,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    )
    .await;
    notify(
        &mut service,
        json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await;

    let uri = "file:///workspace/ok/stack.toml";
    notify(
        &mut service,
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

    let mut empty_diags_seen = false;
    for _ in 0..16 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            futures_util::StreamExt::next(&mut messages),
        )
        .await
        {
            Ok(Some(msg)) => {
                let v = serde_json::to_value(&msg).unwrap();
                if v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == uri {
                    let diags = v["params"]["diagnostics"].as_array().unwrap();
                    assert!(diags.is_empty(), "expected no diags, got {diags:?}");
                    empty_diags_seen = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(empty_diags_seen, "did not observe a publishDiagnostics");
}
