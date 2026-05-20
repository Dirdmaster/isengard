//! Language server for the Isengard stack vocabulary.
//!
//! Bundled as `isd lsp`. Reads/writes LSP over stdio against the editor.
//! Phase 1 (this scaffold) advertises only the `initialize` / `initialized`
//! / `shutdown` lifecycle so editors can attach without crashing; later
//! phases layer diagnostics, hover, and completion on top.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-20-isengard-lsp-design.md`
//! in the operator's vault.

#![forbid(unsafe_code)]

mod server;

pub use server::run_stdio;

#[doc(hidden)]
pub fn test_backend(client: tower_lsp::Client) -> server::Backend {
    server::Backend::new(client)
}
