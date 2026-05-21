//! Static validation for `stack.toml` and `isengard.toml`. Reuses the
//! `isengard-manifest` parser; the LSP side translates
//! [`isengard_manifest::ManifestError`] into LSP [`Diagnostic`]s with
//! file:line ranges.
//!
//! Lives alongside [`crate::diagnostics`] (which handles
//! `compose.yaml`). The top-level [`crate::diagnostics::diagnose`]
//! dispatcher delegates here for [`DocKind::StackToml`] and
//! [`DocKind::FleetToml`] documents.

use std::path::PathBuf;

use isengard_manifest::{ManifestError, parse_fleet_manifest, parse_stack_manifest};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Url};

use crate::diagnostics::SOURCE;
use crate::document::{DocKind, Document};

/// Run the right TOML validator for `doc` against `uri` and return the
/// diagnostics. Non-TOML kinds fall through to an empty list so callers
/// can dispatch unconditionally.
pub fn diagnose(uri: &Url, doc: &Document) -> Vec<Diagnostic> {
    match doc.kind {
        DocKind::StackToml => validate_stack_toml(uri, doc),
        DocKind::FleetToml => validate_fleet_toml(doc),
        _ => Vec::new(),
    }
}

fn validate_stack_toml(uri: &Url, doc: &Document) -> Vec<Diagnostic> {
    // `parse_stack_manifest` takes a `root` PathBuf used to enforce
    // relative-path checks on `compose:` entries. Synthesize one from
    // the URI's parent so the validator runs as the operator would
    // experience it. The fallback (".") keeps tests with abstract URIs
    // working.
    let root = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    match parse_stack_manifest(&doc.text, root) {
        Ok(_) => Vec::new(),
        Err(e) => vec![manifest_error_to_diagnostic(e, doc)],
    }
}

fn validate_fleet_toml(doc: &Document) -> Vec<Diagnostic> {
    match parse_fleet_manifest(&doc.text) {
        Ok(_) => Vec::new(),
        Err(e) => vec![manifest_error_to_diagnostic(e, doc)],
    }
}

/// Map a [`ManifestError`] to an LSP [`Diagnostic`]. TOML syntax errors
/// carry a byte span; structural errors (missing fields, bad enum
/// values) don't, so we anchor those to the first line of the file as
/// a fallback the operator can still navigate to.
fn manifest_error_to_diagnostic(err: ManifestError, doc: &Document) -> Diagnostic {
    let message = err.to_string();
    let range = match &err {
        ManifestError::Toml {
            span: Some(span), ..
        } => doc.range(span.clone()),
        _ => doc.first_line_range(),
    };
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(SOURCE.into()),
        message,
        ..Diagnostic::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with(kind: DocKind, text: &str) -> Document {
        Document {
            text: text.to_string(),
            kind,
            version: 1,
        }
    }

    fn stack_uri() -> Url {
        Url::parse("file:///workspace/hello/stack.toml").unwrap()
    }

    fn fleet_uri() -> Url {
        Url::parse("file:///workspace/isengard.toml").unwrap()
    }

    #[test]
    fn valid_stack_toml_emits_no_diagnostics() {
        let text = r#"
name = "hello"
compose = ["compose.yaml"]
"#;
        let d = doc_with(DocKind::StackToml, text);
        assert!(diagnose(&stack_uri(), &d).is_empty());
    }

    #[test]
    fn missing_name_field_surfaces_diagnostic() {
        let text = r#"
compose = ["compose.yaml"]
"#;
        let d = doc_with(DocKind::StackToml, text);
        let diags = diagnose(&stack_uri(), &d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("name"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("isengard"));
    }

    #[test]
    fn toml_syntax_error_has_span() {
        let text = "name = \nbroken";
        let d = doc_with(DocKind::StackToml, text);
        let diags = diagnose(&stack_uri(), &d);
        assert_eq!(diags.len(), 1);
        // The span should point somewhere into the file, not the (0,0) fallback.
        // We can't pin the exact location across `toml` versions, but
        // we can require something past the start.
        let pos = diags[0].range.start;
        assert!(
            pos.line > 0 || pos.character > 0,
            "expected non-zero position, got {pos:?}"
        );
    }

    #[test]
    fn valid_fleet_toml_emits_no_diagnostics() {
        let text = r#"
fleet = "default"
"#;
        let d = doc_with(DocKind::FleetToml, text);
        assert!(diagnose(&fleet_uri(), &d).is_empty());
    }

    #[test]
    fn other_kinds_emit_nothing() {
        let d = doc_with(DocKind::Other, "totally invalid toml = =");
        let uri = Url::parse("file:///somewhere/Cargo.toml").unwrap();
        assert!(diagnose(&uri, &d).is_empty());
    }
}
