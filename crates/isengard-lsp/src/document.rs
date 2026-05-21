//! In-memory document store. Holds the text the editor sent us so we can
//! re-parse on every change and walk it for diagnostics + hover + completion.
//!
//! The model is the simplest thing that works for Phase 3: full text per
//! URI, swapped on every `did_change`. Incremental edits are reconstructed
//! into the new full text by the LSP layer before reaching this store.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Position, Range, Url};

/// Kind of file we recognise. Drives which parser the diagnostics pipeline
/// runs against the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// `compose.yaml`, `compose.yml`, or a compose overlay (`compose.*.yaml`).
    Compose,
    /// `stack.toml`: per-stack manifest validated by
    /// `isengard_manifest::parse_stack_manifest`.
    StackToml,
    /// `isengard.toml`: per-fleet manifest validated by
    /// `isengard_manifest::parse_fleet_manifest`.
    FleetToml,
    /// Any other file the editor opened. Kept in the store so future
    /// phases can flip on diagnostics without re-plumbing did_open.
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

impl Document {
    /// Translate a byte offset into the document into an LSP [`Position`].
    /// Out-of-range offsets clamp to the end of the document. LSP columns
    /// are UTF-16 code units, not bytes, so multi-byte chars on the same
    /// line get counted by their UTF-16 length.
    pub fn position(&self, byte_offset: usize) -> Position {
        let clamped = byte_offset.min(self.text.len());
        let mut line: u32 = 0;
        let mut line_start: usize = 0;
        for (i, b) in self.text.as_bytes().iter().enumerate() {
            if i >= clamped {
                break;
            }
            if *b == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }
        let utf16_offset: u32 = self.text[line_start..clamped]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        Position {
            line,
            character: utf16_offset,
        }
    }

    /// Convert a byte span into an LSP [`Range`]. Inclusive start,
    /// exclusive end; an empty span renders as a zero-width caret.
    pub fn range(&self, byte_span: std::ops::Range<usize>) -> Range {
        Range {
            start: self.position(byte_span.start),
            end: self.position(byte_span.end),
        }
    }

    /// Range covering the first line of the document. Used as a fallback
    /// when a validator surfaces an error with no span (e.g. a missing
    /// required field) so the diagnostic still has somewhere to land.
    pub fn first_line_range(&self) -> Range {
        let end = self
            .text
            .as_bytes()
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(self.text.len());
        self.range(0..end)
    }
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
/// We accept:
///
/// - `compose.yaml`, `compose.yml`, and the overlay pattern
///   `compose.<anything>.yaml` / `compose.<anything>.yml` as
///   [`DocKind::Compose`].
/// - `stack.toml` as [`DocKind::StackToml`].
/// - `isengard.toml` as [`DocKind::FleetToml`].
///
/// Everything else (unrelated YAML files like `values.yaml`, generic
/// `Cargo.toml`, etc.) becomes [`DocKind::Other`] and stays out of the
/// diagnostics pipeline.
pub fn detect_kind(uri: &Url) -> DocKind {
    let Some(mut segments) = uri.path_segments() else {
        return DocKind::Other;
    };
    let Some(name) = segments.next_back() else {
        return DocKind::Other;
    };
    if is_compose_basename(name) {
        DocKind::Compose
    } else if name == "stack.toml" {
        DocKind::StackToml
    } else if name == "isengard.toml" {
        DocKind::FleetToml
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
    fn detect_toml_manifests() {
        assert_eq!(detect_kind(&uri("/repo/stack.toml")), DocKind::StackToml);
        assert_eq!(
            detect_kind(&uri("/repo/services/web/stack.toml")),
            DocKind::StackToml
        );
        assert_eq!(detect_kind(&uri("/repo/isengard.toml")), DocKind::FleetToml);
    }

    #[test]
    fn position_at_start_of_first_line_is_zero_zero() {
        let doc = Document {
            text: "hello\nworld\n".into(),
            kind: DocKind::Other,
            version: 1,
        };
        let pos = doc.position(0);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn position_after_newline_advances_line() {
        let doc = Document {
            text: "hello\nworld\n".into(),
            kind: DocKind::Other,
            version: 1,
        };
        let pos = doc.position(6);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn position_inside_a_line_uses_utf16_chars() {
        // `é` is 2 bytes in UTF-8 but 1 UTF-16 code unit.
        let doc = Document {
            text: "héllo".into(),
            kind: DocKind::Other,
            version: 1,
        };
        let pos = doc.position(3); // byte offset of `l`
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 2);
    }

    #[test]
    fn out_of_range_offset_clamps_to_end() {
        let doc = Document {
            text: "abc".into(),
            kind: DocKind::Other,
            version: 1,
        };
        let pos = doc.position(999);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 3);
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
