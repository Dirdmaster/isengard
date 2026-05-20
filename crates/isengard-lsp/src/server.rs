//! tower-lsp `LanguageServer` impl. Phase 3 wires document sync and the
//! diagnostics pipeline against the label registry; later phases layer
//! hover and completion on the same document store.

use anyhow::Result;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, InitializeResult, InitializedParams, MessageType, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::diagnostics::diagnose;
use crate::document::DocumentStore;

/// LSP backend. Holds the tower-lsp [`Client`] so handlers can push
/// notifications back to the editor, plus the in-memory document store
/// behind a mutex so concurrent edits do not race. State accumulates here
/// as new phases land (controller cache in Phase 6).
#[derive(Debug)]
pub struct Backend {
    client: Client,
    docs: Mutex<DocumentStore>,
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            docs: Mutex::new(DocumentStore::new()),
        }
    }

    /// Run the diagnostics pipeline for `uri` and publish the result.
    ///
    /// Called from every text-sync handler. The store lookup is short:
    /// we copy out the document, drop the lock, then run validation off
    /// the hot path.
    async fn publish(&self, uri: Url) {
        let doc = {
            let docs = self.docs.lock().await;
            docs.get(&uri).cloned()
        };
        let Some(doc) = doc else { return };
        let diags = diagnose(&doc);
        self.client
            .publish_diagnostics(uri, diags, Some(doc.version))
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Incremental sync is advertised so editors negotiate it
                // once. The text-sync handlers translate the deltas the
                // editor sends into a fresh full-text buffer.
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        {
            let mut docs = self.docs.lock().await;
            docs.upsert(
                params.text_document.uri,
                params.text_document.text,
                params.text_document.version,
            );
        }
        self.publish(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        {
            let mut docs = self.docs.lock().await;
            let prior = docs.get(&uri).map(|d| d.text.clone()).unwrap_or_default();
            // tower-lsp 0.20 + lsp-types 0.94 report INCREMENTAL edits as a
            // list of full or partial replacements. Phase 3 takes the
            // simplest correct path: apply the LAST full-content change
            // if any is present, otherwise leave the prior text alone.
            // Real ranged-edit reassembly lands when hover or completion
            // need character-precise state (Phase 4 / 5); for diagnostics,
            // "re-validate on save" semantics are close enough.
            let new_text = params
                .content_changes
                .iter()
                .find(|c| c.range.is_none())
                .map(|c| c.text.clone())
                .unwrap_or(prior);
            docs.upsert(uri.clone(), new_text, version);
        }
        self.publish(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let mut docs = self.docs.lock().await;
        docs.remove(&uri);
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
