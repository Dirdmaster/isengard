//! Language server for the Isengard stack vocabulary.
//!
//! Bundled as `isd lsp`. Reads/writes LSP over stdio against the editor.
//! Two diagnostic surfaces live in this crate:
//!
//! - [`diagnostics`] + [`yaml`] validate `labels:` entries in a
//!   `compose.yaml` document against the [`registry`] of every
//!   `isengard.*` label the platform recognises.
//! - [`toml_diagnostics`] validates `stack.toml` and `isengard.toml`
//!   through `isengard_manifest::parse`; TOML syntax errors with a
//!   byte span are pinned at the exact range in the editor.
//!
//! The top-level dispatcher is [`diagnostics::diagnose`], which routes
//! by [`document::DocKind`]. Phase 4 layers hover docs on the registry;
//! Phase 5 layers completion.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-20-isengard-lsp-design.md`
//! in the operator's vault.

#![forbid(unsafe_code)]

pub mod diagnostics;
pub mod document;
pub mod registry;
mod server;
pub mod toml_diagnostics;
pub mod yaml;

pub use server::run_stdio;

#[doc(hidden)]
pub fn test_backend(client: tower_lsp::Client) -> server::Backend {
    server::Backend::new(client)
}
