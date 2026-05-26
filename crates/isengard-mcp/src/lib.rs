//! Model Context Protocol server for Isengard.
//!
//! Bundled as `isd mcp`. Speaks MCP over stdio for assistant clients.
//! The server advertises one capability in v1:
//!
//! - `resources/list` + `resources/read` for operator-facing docs
//!   under `docs/**` and per-crate API reference under
//!   `crates/<crate>/docs/**`.
//!
//! No prompts or `tools/*` surface in v1. Exposing destructive operations
//! (`isd init`, `isd stack deploy`) as MCP tools needs its own scoping
//! design and is intentionally deferred.

#![forbid(unsafe_code)]

mod embedded;
mod resources;
mod server;
mod uri;

pub use server::run_stdio;

#[doc(hidden)]
pub use embedded::{API_DOCS, OPERATOR_DOCS};
#[doc(hidden)]
pub use resources::{ResourceEntry, list_resources, read_resource};
#[doc(hidden)]
pub use uri::{ResourceUri, build_api_uri, build_docs_uri};
