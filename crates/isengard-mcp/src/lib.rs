//! Model Context Protocol server for Isengard.
//!
//! Bundled as `isd mcp`. Speaks MCP over stdio against an AI host
//! (Claude Code, any MCP-capable LLM). The server advertises three
//! capabilities in v1:
//!
//! - `resources/list` + `resources/read` for operator-facing docs
//!   under `docs/**` and per-crate API reference under
//!   `crates/<crate>/docs/**`.
//! - `prompts/list` + `prompts/get` for AI playbooks under
//!   `skills/**`. Each skill's YAML front-matter declares the
//!   parameters the host should solicit.
//!
//! No `tools/*` surface in v1. Exposing destructive operations
//! (`isd init`, `isd stack up`) as MCP tools needs its own scoping
//! design; that's deferred.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-20-isengard-docs-and-ai-surface-design.md`
//! and the Phase 4 task list in `2026-05-20-isengard-docs-and-ai-surface.md`.

#![forbid(unsafe_code)]

mod embedded;
mod prompts;
mod resources;
mod server;
mod uri;

pub use server::run_stdio;

#[doc(hidden)]
pub use embedded::{API_DOCS, OPERATOR_DOCS, SKILLS};
#[doc(hidden)]
pub use prompts::{ParsedSkill, list_skills, render_prompt};
#[doc(hidden)]
pub use resources::{ResourceEntry, list_resources, read_resource};
#[doc(hidden)]
pub use uri::{ResourceUri, build_api_uri, build_docs_uri, build_skill_uri};
