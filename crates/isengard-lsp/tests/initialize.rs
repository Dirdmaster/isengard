//! Drive the LSP service in-process: send an `initialize` request,
//! expect a sensible capabilities reply. Locks the handshake contract
//! so a future refactor of `Backend` can't silently regress it.

use serde_json::json;
use tower::{Service, ServiceExt};
use tower_lsp::LspService;

#[tokio::test]
async fn initialize_replies_with_server_info() {
    let (service, _socket) = LspService::new(isengard_lsp::test_backend);
    let mut service = service;

    // jsonrpc::Request is private; ship a raw JSON-RPC envelope. tower-lsp's
    // service handles framing internally and emits a typed Response we can
    // serialize back to JSON for assertions.
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {} }
    });
    let request: tower_lsp::jsonrpc::Request = serde_json::from_value(payload).unwrap();

    let response = service
        .ready()
        .await
        .unwrap()
        .call(request)
        .await
        .unwrap()
        .expect("server should reply to initialize");

    let body = serde_json::to_value(&response).unwrap();
    let name = body["result"]["serverInfo"]["name"]
        .as_str()
        .expect("serverInfo.name in response");
    assert_eq!(name, "isengard-lsp");

    // Phase 1 contract: incremental text document sync is advertised so
    // editors negotiate it once; later phases consume the deltas.
    let sync_kind = &body["result"]["capabilities"]["textDocumentSync"];
    // tower-lsp serializes TextDocumentSyncKind::INCREMENTAL as the integer 2.
    assert_eq!(sync_kind, &json!(2));
}
