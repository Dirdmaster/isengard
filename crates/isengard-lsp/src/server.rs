//! tower-lsp `LanguageServer` impl. Phase 1 advertises only the
//! lifecycle methods so the handshake completes cleanly; diagnostics
//! and the rest of the surface land in subsequent PRs.

use anyhow::Result;
use tower_lsp::jsonrpc;
use tower_lsp::lsp_types::{
    InitializeParams, InitializeResult, InitializedParams, MessageType, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// LSP backend. Holds the tower-lsp `Client` so handlers can push
/// notifications (log messages, diagnostics) back to the editor. State
/// accumulates here as new phases land (document store in Phase 2,
/// controller cache in Phase 6).
#[derive(Debug)]
pub struct Backend {
    client: Client,
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Phase 2 will need text deltas to track edits; advertise
                // incremental sync now so editors negotiate it once.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "isengard-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "isengard-lsp ready")
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }
}

/// Start the language server on stdio and block until the client
/// disconnects. The editor (Neovim via `isengard.nvim`, VSCode later)
/// spawns `isd lsp` as a subprocess; this function owns its lifetime.
pub async fn run_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}
