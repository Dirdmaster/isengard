//! Language server for the Isengard stack vocabulary.
//!
//! Bundled as `isd lsp`. Reads/writes LSP over stdio against the editor.
//! Phase 3 (this drop) adds the [`registry`] of every `isengard.*` label
//! the platform recognises and a YAML diagnostics pipeline that validates
//! every `labels:` entry on a compose document. Phase 4 layers hover docs
//! on the same registry; Phase 5 layers completion.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-20-isengard-lsp-design.md`
//! in the operator's vault.

#![forbid(unsafe_code)]

pub mod diagnostics;
pub mod document;
pub mod registry;
mod server;
pub mod yaml;

pub use server::run_stdio;

#[doc(hidden)]
pub fn test_backend(client: tower_lsp::Client) -> server::Backend {
    server::Backend::new(client)
}
