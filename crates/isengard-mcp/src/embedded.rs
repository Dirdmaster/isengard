//! Compile-time embedded markdown trees.
//!
//! `include_dir!` reads each directory at build time and bakes every
//! file into the binary's data section. The MCP server walks these
//! statics at runtime to enumerate resources and prompts. There is
//! no filesystem dependency at runtime; the binary is self-contained.
//!
//! Two trees:
//!
//! - [`OPERATOR_DOCS`] is the operator-facing guide tree under
//!   `docs/` (getting-started, concepts, reference, recipes).
//! - [`API_DOCS`] is a build-time mirror of every `crates/<crate>/docs/`
//!   (and nested `crates/<group>/<crate>/docs/`) subtree. `build.rs`
//!   stages the mirror under `$OUT_DIR/api_docs/` before this macro
//!   runs, so the binary embeds only markdown reference, not the
//!   surrounding Rust source.
//!
//! Future contributor workflow (out of scope for v1): a
//! `$ISD_DOCS_DIR` override would let contributors hack on the
//! markdown without recompiling. The plan tracks that as a Phase 5+
//! polish item.

use include_dir::{Dir, include_dir};

/// Operator-facing guides. Mounted at `isengard://docs/<path>`.
pub static OPERATOR_DOCS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../docs");

/// Build-time mirror of every per-crate `docs/` subtree. `build.rs`
/// stages this under `$OUT_DIR/api_docs/` so the layout matches what
/// the resource enumerator expects: `<crate>/docs/<file>.md` for
/// top-level crates and `<group>/<plugin>/docs/<file>.md` for nested
/// plugin crates. Exposed at `isengard://api/<crate>/<symbol>`.
pub static API_DOCS: Dir<'_> = include_dir!("$OUT_DIR/api_docs");
