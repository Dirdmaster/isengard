//! Compile-time embedded markdown trees.
//!
//! `include_dir!` reads each directory at build time and bakes every
//! file into the binary's data section. The MCP server walks these
//! statics at runtime to enumerate resources and prompts. There is
//! no filesystem dependency at runtime; the binary is self-contained.
//!
//! Three trees:
//!
//! - [`OPERATOR_DOCS`] is the operator-facing guide tree under
//!   `docs/` (getting-started, concepts, reference, recipes).
//! - [`API_DOCS`] is the workspace's `crates/` tree. The resource
//!   enumerator only surfaces files under any `crates/<crate>/docs/`
//!   subtree; the rest of the source comes along for the ride.
//! - [`SKILLS`] is the top-level `skills/` tree, one markdown file
//!   per AI playbook.
//!
//! Future contributor workflow (out of scope for v1): a
//! `$ISD_DOCS_DIR` override would let contributors hack on the
//! markdown without recompiling. The plan tracks that as a Phase 5+
//! polish item.

use include_dir::{Dir, include_dir};

/// Operator-facing guides. Mounted at `isengard://docs/<path>`.
pub static OPERATOR_DOCS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../docs");

/// Workspace `crates/` tree. The MCP resource enumerator filters this
/// down to `*/docs/*.md` so only API reference is exposed at
/// `isengard://api/<crate>/<symbol>`.
pub static API_DOCS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../crates");

/// AI playbooks. Mounted at `isengard://skill/<name>`.
pub static SKILLS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../skills");
