//! `rmcp::ServerHandler` impl that wires the embedded trees into the
//! MCP protocol surface.
//!
//! Capabilities advertised: `resources`. No prompts or `tools/*` in
//! v1. Initialization returns the cargo package version so clients can
//! pin against a known protocol revision.

use std::future::Future;
use std::sync::OnceLock;

use anyhow::Result;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    model::{
        AnnotateAble, Implementation, InitializeRequestParams, InitializeResult,
        ListResourcesResult, PaginatedRequestParams, ProtocolVersion, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ResourcesCapability,
        ServerCapabilities,
    },
    service::{MaybeSendFuture, RequestContext},
    transport::io::stdio,
};

use crate::resources::{list_resources, read_resource};

/// MCP server state. The embedded trees are `static`, so cloning the
/// backend is cheap.
#[derive(Debug, Clone, Default)]
pub struct Backend;

impl Backend {
    /// Build a docs-only backend.
    pub fn new() -> Self {
        Self
    }
}

impl ServerHandler for Backend {
    fn get_info(&self) -> InitializeResult {
        let mut caps = ServerCapabilities::default();
        caps.resources = Some(ResourcesCapability {
            subscribe: Some(false),
            list_changed: Some(false),
        });
        let mut server_info = Implementation::default();
        server_info.name = "isengard-mcp".into();
        server_info.title = Some("Isengard".into());
        server_info.version = env!("CARGO_PKG_VERSION").into();
        server_info.description =
            Some("Embedded operator docs and per-crate API reference for Isengard.".into());
        server_info.website_url = Some("https://isengard.app".into());
        InitializeResult::new(caps)
            .with_server_info(server_info)
            .with_protocol_version(ProtocolVersion::default())
            .with_instructions(
                "Use `resources/list` to discover operator guides at `isengard://docs/*` and per-crate API reference at `isengard://api/<crate>/*`.",
            )
    }

    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        // Match the default impl: stash the client info so later
        // notifications can introspect it.
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        std::future::ready(Ok(self.get_info()))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        let resources = list_resources()
            .into_iter()
            .map(|entry| {
                let raw = RawResource::new(entry.uri, entry.name).with_mime_type("text/markdown");
                raw.no_annotation()
            })
            .collect::<Vec<_>>();
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        let uri = request.uri.clone();
        let result = match read_resource(&uri) {
            Some(body) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(body.to_string(), uri).with_mime_type("text/markdown"),
            ])),
            None => Err(rmcp::ErrorData::invalid_params(
                format!("unknown isengard:// resource: {uri}"),
                None,
            )),
        };
        std::future::ready(result)
    }
}

/// Process-wide backend cache. The first `run_stdio` call builds and
/// stashes one; subsequent ones reuse it.
static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Start the MCP server on stdio and block until the host
/// disconnects.
///
/// Clients spawn `isd mcp` as a subprocess; this function owns its
/// lifetime. The handler returns on EOF (host shut down) or on a
/// transport error.
pub async fn run_stdio() -> Result<()> {
    let backend = BACKEND.get_or_init(Backend::new).clone();
    let running = backend
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP transport init failed: {e}"))?;
    running
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server task panicked: {e}"))?;
    Ok(())
}
