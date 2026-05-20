//! `rmcp::ServerHandler` impl that wires the embedded trees into the
//! MCP protocol surface.
//!
//! Capabilities advertised: `resources` and `prompts`. No `tools/*`
//! in v1 (see the spec's "Out of scope" section). Initialization
//! returns the cargo package version so AI hosts can pin against a
//! known protocol revision.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::OnceLock;

use anyhow::Result;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    model::{
        AnnotateAble, GetPromptRequestParams, GetPromptResult, Implementation,
        InitializeRequestParams, InitializeResult, ListPromptsResult, ListResourcesResult,
        PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, PromptMessageContent,
        PromptMessageRole, PromptsCapability, ProtocolVersion, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ResourcesCapability,
        ServerCapabilities,
    },
    service::{MaybeSendFuture, RequestContext},
    transport::io::stdio,
};

use crate::prompts::{ParsedSkill, list_skills, render_prompt};
use crate::resources::{list_resources, read_resource};

/// MCP server state. Holds the parsed skill catalogue so
/// `prompts/list` and `prompts/get` are O(1) lookups.
///
/// Constructed once per `isd mcp` invocation. The embedded trees are
/// `static`, so cloning the backend is cheap (an `Arc`-like clone of
/// the skill list).
#[derive(Debug, Clone)]
pub struct Backend {
    skills: std::sync::Arc<Vec<ParsedSkill>>,
}

impl Backend {
    /// Build a backend from the embedded skills tree. Front-matter
    /// is parsed eagerly so malformed skills surface a warning at
    /// startup, not on the first `prompts/list`.
    pub fn new() -> Self {
        Self {
            skills: std::sync::Arc::new(list_skills()),
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHandler for Backend {
    fn get_info(&self) -> InitializeResult {
        let mut caps = ServerCapabilities::default();
        caps.resources = Some(ResourcesCapability {
            subscribe: Some(false),
            list_changed: Some(false),
        });
        caps.prompts = Some(PromptsCapability {
            list_changed: Some(false),
        });
        let mut server_info = Implementation::default();
        server_info.name = "isengard-mcp".into();
        server_info.title = Some("Isengard".into());
        server_info.version = env!("CARGO_PKG_VERSION").into();
        server_info.description = Some(
            "Embedded operator docs, per-crate API reference, and AI playbooks for Isengard."
                .into(),
        );
        server_info.website_url = Some("https://isengard.app".into());
        InitializeResult::new(caps)
            .with_server_info(server_info)
            .with_protocol_version(ProtocolVersion::default())
            .with_instructions(
                "Use `resources/list` to discover operator guides at `isengard://docs/*`, per-crate API reference at `isengard://api/<crate>/*`, and AI playbooks at `isengard://skill/*`. Use `prompts/list` to enumerate skills with declared parameters.",
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

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, rmcp::ErrorData>> + MaybeSendFuture + '_
    {
        let prompts: Vec<Prompt> = self
            .skills
            .iter()
            .map(|skill| {
                let arguments = skill
                    .parameters
                    .iter()
                    .map(|p| {
                        let mut arg = PromptArgument::new(&p.name).with_required(p.required);
                        if let Some(desc) = p.description.as_deref() {
                            arg = arg.with_description(desc);
                        }
                        arg
                    })
                    .collect::<Vec<_>>();
                let mut prompt = Prompt::new(
                    skill.name.clone(),
                    skill.title.clone(),
                    if arguments.is_empty() {
                        None
                    } else {
                        Some(arguments)
                    },
                );
                if let Some(title) = skill.title.as_deref() {
                    prompt = prompt.with_title(title);
                }
                prompt
            })
            .collect();
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResult, rmcp::ErrorData>> + MaybeSendFuture + '_ {
        let name = request.name.clone();
        let arguments: BTreeMap<String, String> = request
            .arguments
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                serde_json::Value::Number(n) => Some((k, n.to_string())),
                serde_json::Value::Bool(b) => Some((k, b.to_string())),
                _ => None,
            })
            .collect();
        let result = match self.skills.iter().find(|s| s.name == name) {
            Some(skill) => {
                let body = render_prompt(skill, &arguments);
                let message =
                    PromptMessage::new(PromptMessageRole::User, PromptMessageContent::text(body));
                let mut out = GetPromptResult::new(vec![message]);
                if let Some(title) = skill.title.as_deref() {
                    out = out.with_description(title);
                }
                Ok(out)
            }
            None => Err(rmcp::ErrorData::invalid_params(
                format!("unknown skill: {name}"),
                None,
            )),
        };
        std::future::ready(result)
    }
}

/// Process-wide backend cache. The first `run_stdio` call builds and
/// stashes one; subsequent ones reuse it. Skill front-matter parsing
/// runs once per process.
static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Start the MCP server on stdio and block until the host
/// disconnects.
///
/// Editor/AI hosts spawn `isd mcp` as a subprocess; this function
/// owns its lifetime. The handler returns on EOF (host shut down)
/// or on a transport error.
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
