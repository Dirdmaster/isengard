//! `resources/list` and `resources/read` over the embedded trees.
//!
//! Resources are enumerated from two roots:
//!
//! - [`OPERATOR_DOCS`] walks `docs/**/*.md` recursively. Every `.md`
//!   file becomes one resource at `isengard://docs/<relative-path>`.
//! - [`API_DOCS`] walks `crates/*/docs/**/*.md`. The first segment
//!   under `crates/` is the crate name; the file under that crate's
//!   `docs/` becomes one resource at
//!   `isengard://api/<crate>/<stem>`. Source files (`*.rs`,
//!   `Cargo.toml`) bundled by `include_dir!` are filtered out here.
//!
//! Files whose name starts with `_` (e.g. `_dir.yml`) are skipped:
//! those are Docus navigation manifests, not content. Files whose
//! name is `.gitkeep` are skipped for the same reason.

use std::path::Path;

use include_dir::{Dir, DirEntry};

use crate::embedded::{API_DOCS, OPERATOR_DOCS};
use crate::uri::{build_api_uri, build_docs_uri};

/// One entry in the resource catalogue.
///
/// The `uri` is the canonical `isengard://` reference. `name` is a
/// short human-readable label; `description` is the optional one-line
/// summary (currently `None` until front-matter parsing lands for
/// docs in Phase 5+). `body` is the raw markdown bytes.
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    /// Canonical `isengard://` URI.
    pub uri: String,
    /// Short human-readable label, derived from the file stem.
    pub name: String,
    /// Raw markdown body.
    pub body: &'static str,
}

/// Walk the embedded trees and return every exposed resource.
///
/// Order is stable per build (the order `include_dir!` walks the tree).
pub fn list_resources() -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    collect_docs(&OPERATOR_DOCS, Path::new(""), &mut out);
    collect_api(&API_DOCS, &mut out);
    out
}

/// Look up one resource by its `isengard://` URI. Returns `None` when
/// the URI is unknown or its referenced file is not present in the
/// embedded tree.
pub fn read_resource(uri: &str) -> Option<&'static str> {
    use crate::uri::ResourceUri;

    match ResourceUri::parse(uri)? {
        ResourceUri::Docs(path) => {
            let file_path = format!("{path}.md");
            OPERATOR_DOCS
                .get_file(&file_path)
                .and_then(|f| f.contents_utf8())
        }
        ResourceUri::Api { krate, symbol } => {
            // Try the workspace-member layout first
            // (`crates/<crate>/docs/<symbol>.md`). When the URI was
            // minted for a plugin, the crate directory is
            // `isengard-plugins/<plugin>/docs/`; fall through to that
            // layout when the direct lookup misses.
            let direct = format!("{krate}/docs/{symbol}.md");
            if let Some(body) = API_DOCS.get_file(&direct).and_then(|f| f.contents_utf8()) {
                return Some(body);
            }
            let plugin = format!("isengard-plugins/{krate}/docs/{symbol}.md");
            API_DOCS.get_file(&plugin).and_then(|f| f.contents_utf8())
        }
    }
}

/// Walk `docs/`. Every `.md` file (except `_*` and `.gitkeep`)
/// becomes one resource.
fn collect_docs(dir: &Dir<'static>, _prefix: &Path, out: &mut Vec<ResourceEntry>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(sub) => collect_docs(sub, sub.path(), out),
            DirEntry::File(file) => {
                let path = file.path();
                if !is_markdown(path) {
                    continue;
                }
                if let Some(body) = file.contents_utf8() {
                    let rel = path.to_string_lossy().to_string();
                    let uri = build_docs_uri(&rel);
                    let name = friendly_name(path);
                    out.push(ResourceEntry { uri, name, body });
                }
            }
        }
    }
}

/// Walk `crates/`. Only files under `crates/<crate>/docs/**/*.md`
/// surface as resources; everything else (source, manifests) is
/// ignored. The crate name is the first path segment; the file
/// stem under `docs/` is the symbol.
fn collect_api(dir: &Dir<'static>, out: &mut Vec<ResourceEntry>) {
    for crate_entry in dir.entries() {
        let DirEntry::Dir(crate_dir) = crate_entry else {
            continue;
        };
        let crate_name = match crate_dir.path().file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Handle plugins/<plugin> too: recurse one level when no
        // direct `docs/` child exists at this crate root.
        if let Some(docs_dir) = crate_dir.get_dir(format!("{crate_name}/docs").as_str()) {
            // Should not happen with workspace layout; fallthrough for safety.
            collect_api_docs(crate_name, docs_dir, out);
            continue;
        }
        // Look for `<crate>/docs` subdirectory.
        let docs_path = crate_dir.path().join("docs");
        if let Some(docs_dir) = API_DOCS.get_dir(&docs_path) {
            collect_api_docs(crate_name, docs_dir, out);
        }
        // Plugins live under `crates/isengard-plugins/<plugin>/docs`.
        // Recurse one level: enumerate child directories of the
        // top-level entry and treat each as its own crate.
        if crate_name == "isengard-plugins" {
            for plugin_entry in crate_dir.entries() {
                let DirEntry::Dir(plugin_dir) = plugin_entry else {
                    continue;
                };
                let plugin_name = match plugin_dir.path().file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                let plugin_docs_path = plugin_dir.path().join("docs");
                if let Some(docs_dir) = API_DOCS.get_dir(&plugin_docs_path) {
                    collect_api_docs(plugin_name, docs_dir, out);
                }
            }
        }
    }
}

fn collect_api_docs(crate_name: &str, docs_dir: &Dir<'static>, out: &mut Vec<ResourceEntry>) {
    for entry in docs_dir.entries() {
        let DirEntry::File(file) = entry else {
            continue;
        };
        let path = file.path();
        if !is_markdown(path) {
            continue;
        }
        let symbol = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if let Some(body) = file.contents_utf8() {
            let uri = build_api_uri(crate_name, symbol);
            let name = format!("{crate_name}::{symbol}");
            out.push(ResourceEntry { uri, name, body });
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("md") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == ".gitkeep" {
        return false;
    }
    if name.starts_with('_') {
        return false;
    }
    true
}

fn friendly_name(path: &Path) -> String {
    path.with_extension("")
        .to_string_lossy()
        .replace('/', " / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_at_least_one_operator_doc() {
        let entries = list_resources();
        let docs: Vec<_> = entries
            .iter()
            .filter(|e| e.uri.starts_with("isengard://docs/"))
            .collect();
        assert!(
            !docs.is_empty(),
            "expected at least one operator doc resource",
        );
    }

    #[test]
    fn lists_at_least_one_api_doc() {
        let entries = list_resources();
        let api: Vec<_> = entries
            .iter()
            .filter(|e| e.uri.starts_with("isengard://api/"))
            .collect();
        assert!(
            !api.is_empty(),
            "expected at least one per-crate API doc resource",
        );
    }

    #[test]
    fn skips_dir_yml_navigation_files() {
        let entries = list_resources();
        for entry in &entries {
            assert!(
                !entry.uri.ends_with("/_dir"),
                "Docus _dir.yml leaked through as a resource: {}",
                entry.uri,
            );
        }
    }

    #[test]
    fn read_resource_round_trips_a_known_doc() {
        // `docs/concepts/labels.md` lands in Phase 1 of the docs plan
        // (PR #196); use it as the known anchor.
        let body = read_resource("isengard://docs/concepts/labels");
        assert!(
            body.is_some(),
            "expected docs/concepts/labels.md to be embedded"
        );
    }

    #[test]
    fn read_resource_returns_none_for_unknown_uri() {
        assert!(read_resource("isengard://docs/does/not/exist").is_none());
        assert!(read_resource("not-a-uri").is_none());
    }
}
