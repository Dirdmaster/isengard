//! In-memory document store. Holds the text the editor sent us so we can
//! re-parse on every change and walk it for diagnostics + hover + completion.
//!
//! The model is the simplest thing that works for Phase 3: full text per
//! URI, swapped on every `did_change`. Incremental edits are reconstructed
//! into the new full text by the LSP layer before reaching this store.

use std::collections::HashMap;

use tower_lsp::lsp_types::Url;

/// Kind of file we recognise. Drives which parser the diagnostics pipeline
/// runs against the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// `compose.yaml`, `compose.yml`, or a compose overlay (`compose.*.yaml`).
    Compose,
    /// Reserved for future TOML support (Phase 2 cleanup). Documents the
    /// store recognises but the LSP currently ignores for diagnostics.
    Other,
}

/// One open document and its parsed-but-not-yet-rendered state.
#[derive(Debug, Clone)]
pub struct Document {
    /// Raw editor text. Always the full document content.
    pub text: String,
    /// Detected file kind. `Other` documents stay in the store so future
    /// phases can flip them on without re-plumbing did_open.
    pub kind: DocKind,
    /// LSP document version. Echoed back when we publish diagnostics so
    /// the editor can drop stale ones.
    pub version: i32,
}

/// Thread-safe handle to the open-document map.
///
/// The LSP backend wraps this in an async mutex; the diagnostics pipeline
/// holds it for short reads only.
#[derive(Debug, Default)]
pub struct DocumentStore {
    docs: HashMap<Url, Document>,
}

impl DocumentStore {
    /// Build an empty store.
    pub fn new() -> Self {
        Self {
            docs: HashMap::new(),
        }
    }

    /// Insert or replace the document at `uri`.
    pub fn upsert(&mut self, uri: Url, text: String, version: i32) {
        let kind = detect_kind(&uri);
        self.docs.insert(
            uri,
            Document {
                text,
                kind,
                version,
            },
        );
    }

    /// Drop the document. Returns the prior entry, if any.
    pub fn remove(&mut self, uri: &Url) -> Option<Document> {
        self.docs.remove(uri)
    }

    /// Borrow a document by URI.
    pub fn get(&self, uri: &Url) -> Option<&Document> {
        self.docs.get(uri)
    }
}

/// Classify a URI by basename.
///
/// We accept `compose.yaml`, `compose.yml`, and the overlay pattern
/// `compose.<anything>.yaml` / `compose.<anything>.yml`. Everything else
/// (including unrelated YAML files like `pyproject.toml`) becomes
/// `DocKind::Other` and stays out of the diagnostics pipeline.
pub fn detect_kind(uri: &Url) -> DocKind {
    let Some(mut segments) = uri.path_segments() else {
        return DocKind::Other;
    };
    let Some(name) = segments.next_back() else {
        return DocKind::Other;
    };
    if is_compose_basename(name) {
        DocKind::Compose
    } else {
        DocKind::Other
    }
}

/// True when `name` matches the compose file convention.
fn is_compose_basename(name: &str) -> bool {
    if name == "compose.yaml" || name == "compose.yml" {
        return true;
    }
    let stem = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"));
    let Some(stem) = stem else { return false };
    stem.starts_with("compose.") && stem.len() > "compose.".len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(path: &str) -> Url {
        Url::parse(&format!("file://{path}")).unwrap()
    }

    #[test]
    fn detect_canonical_compose_names() {
        assert_eq!(detect_kind(&uri("/repo/compose.yaml")), DocKind::Compose);
        assert_eq!(detect_kind(&uri("/repo/compose.yml")), DocKind::Compose);
    }

    #[test]
    fn detect_compose_overlay() {
        assert_eq!(
            detect_kind(&uri("/repo/compose.prod.yaml")),
            DocKind::Compose
        );
        assert_eq!(
            detect_kind(&uri("/repo/compose.staging.yml")),
            DocKind::Compose
        );
    }

    #[test]
    fn rejects_unrelated_yaml() {
        assert_eq!(detect_kind(&uri("/repo/Cargo.toml")), DocKind::Other);
        assert_eq!(detect_kind(&uri("/repo/values.yaml")), DocKind::Other);
        // `compose.yaml` as a substring is not enough; the basename must
        // match.
        assert_eq!(detect_kind(&uri("/repo/not-compose.yaml")), DocKind::Other);
    }

    #[test]
    fn store_round_trip() {
        let mut store = DocumentStore::new();
        let u = uri("/repo/compose.yaml");
        store.upsert(u.clone(), "services: {}\n".into(), 1);
        let doc = store.get(&u).expect("present");
        assert_eq!(doc.kind, DocKind::Compose);
        assert_eq!(doc.version, 1);
        store.remove(&u);
        assert!(store.get(&u).is_none());
    }
}
