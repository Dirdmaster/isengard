//! Diagnostics pipeline. Reads a [`Document`] from the store, walks the
//! YAML for labels, validates each value against the registry, and emits
//! LSP `Diagnostic` values the server can publish.
//!
//! Source string for every diagnostic: `"isengard"`. Severity is
//! [`DiagnosticSeverity::ERROR`] for shape violations (unknown label key,
//! wrong enum variant, port out of range, malformed URL, malformed
//! RFC 3339 timestamp). Later phases layer warnings on top via the live
//! controller cache.

use chrono::DateTime;
use marked_yaml::types::Span;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::{DocKind, Document};
use crate::registry::{ValueKind, lookup};
use crate::yaml::{LabelEntry, find_labels};

/// Source string used in every diagnostic the LSP publishes.
pub const SOURCE: &str = "isengard";

/// Run every static check we know for `doc` and return the diagnostics.
///
/// `Other`-kind documents (anything that is not a compose file) come back
/// with an empty list. Documents whose YAML fails to parse return a single
/// diagnostic pointing at line 1; we keep parse-error coverage minimal in
/// Phase 3, with finer YAML span info layered in once we need it.
pub fn diagnose(doc: &Document) -> Vec<Diagnostic> {
    if doc.kind != DocKind::Compose {
        return Vec::new();
    }
    let entries = match find_labels(&doc.text) {
        Ok(e) => e,
        Err(err) => {
            return vec![Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some(SOURCE.into()),
                message: format!("compose.yaml failed to parse: {err}"),
                ..Diagnostic::default()
            }];
        }
    };
    let mut diags = Vec::new();
    for entry in entries {
        diags.extend(diagnose_entry(&entry));
    }
    diags
}

/// Validate one label entry. Emits at most one diagnostic: an unknown key
/// shortcuts the value check.
fn diagnose_entry(entry: &LabelEntry) -> Vec<Diagnostic> {
    // Ignore Docker / compose internal labels (anything not in the
    // isengard.* / io.isengard.* namespace). Operators commonly mix
    // these with Traefik labels, GitHub labels, etc.
    if !is_isengard_label(&entry.key) {
        return Vec::new();
    }
    let Some(spec) = lookup(&entry.key) else {
        return vec![Diagnostic {
            range: span_to_range(&entry.key_span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.into()),
            message: format!(
                "unknown isengard label `{}` on service `{}`",
                entry.key, entry.service
            ),
            ..Diagnostic::default()
        }];
    };
    if let Some(message) = validate_value(spec.value, &entry.value) {
        return vec![Diagnostic {
            range: span_to_range(&entry.value_span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.into()),
            message: format!("`{}`: {message}", entry.key),
            ..Diagnostic::default()
        }];
    }
    Vec::new()
}

/// True when the label belongs to Isengard's namespace.
fn is_isengard_label(key: &str) -> bool {
    key.starts_with("isengard.") || key.starts_with("io.isengard.")
}

/// Run the [`ValueKind`] check against `value`. Returns `None` on success
/// or a short message describing the violation.
fn validate_value(kind: ValueKind, value: &str) -> Option<String> {
    match kind {
        ValueKind::Enum(variants) => {
            if variants.contains(&value) {
                None
            } else {
                Some(format!(
                    "expected one of [{}], got `{}`",
                    variants.join(", "),
                    value
                ))
            }
        }
        ValueKind::Port => match value.parse::<u32>() {
            Ok(n) if (1..=65535).contains(&n) => None,
            _ => Some(format!("expected port in 1..=65535, got `{value}`")),
        },
        ValueKind::U32 => match value.parse::<u32>() {
            Ok(_) => None,
            Err(_) => Some(format!("expected unsigned 32-bit integer, got `{value}`")),
        },
        ValueKind::Url => {
            if url_is_well_formed(value) {
                None
            } else {
                Some(format!("expected absolute http(s) URL, got `{value}`"))
            }
        }
        ValueKind::Rfc3339 => match DateTime::parse_from_rfc3339(value) {
            Ok(_) => None,
            Err(_) => Some(format!("expected RFC 3339 timestamp, got `{value}`")),
        },
        ValueKind::String => {
            if value.is_empty() {
                Some("expected non-empty string".into())
            } else {
                None
            }
        }
        ValueKind::StringList => {
            if value.is_empty() {
                Some("expected comma-separated list, got empty".into())
            } else if value.split(',').any(|part| part.trim().is_empty()) {
                Some(format!(
                    "comma-separated list has an empty entry: `{value}`"
                ))
            } else {
                None
            }
        }
    }
}

/// Cheap URL well-formedness check. Avoids pulling in the `url` crate just
/// for one validator: require a scheme + `://` + a non-empty host.
fn url_is_well_formed(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty()
}

/// Convert a marked-yaml [`Span`] into the LSP [`Range`] coordinate space.
///
/// marked-yaml lines and columns are 1-indexed; LSP is 0-indexed. A span
/// without a start defaults to the top of the document (the parser
/// guarantees real spans for label keys and values, but we stay defensive).
fn span_to_range(span: &Span) -> Range {
    let start = marker_to_position(span.start());
    let end = span
        .end()
        .map_or_else(|| start, |m| marker_to_position(Some(m)));
    // Editors collapse zero-width ranges; if start == end we widen by one
    // character so the squiggle is visible.
    if start == end {
        Range::new(
            start,
            Position::new(start.line, start.character.saturating_add(1)),
        )
    } else {
        Range::new(start, end)
    }
}

/// Convert one marked-yaml [`Marker`](marked_yaml::Marker) (1-indexed) into
/// an LSP [`Position`] (0-indexed). `None` becomes the origin.
fn marker_to_position(marker: Option<&marked_yaml::Marker>) -> Position {
    match marker {
        Some(m) => Position::new(
            m.line().saturating_sub(1) as u32,
            m.column().saturating_sub(1) as u32,
        ),
        None => Position::new(0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocKind;

    fn doc(text: &str) -> Document {
        Document {
            text: text.into(),
            kind: DocKind::Compose,
            version: 1,
        }
    }

    #[test]
    fn clean_compose_has_no_diagnostics() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.enable: "true"
      isengard.policy.gate: approval
      isengard.expose: plex.vallee.casa
      isengard.expose.port: "8080"
"#);
        assert!(diagnose(&d).is_empty());
    }

    #[test]
    fn unknown_label_flagged() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.fake: nope
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("unknown isengard label"));
    }

    #[test]
    fn enum_value_must_match() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.policy.gate: maybe
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("expected one of"));
    }

    #[test]
    fn port_must_be_in_range() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.expose.port: "70000"
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("port in 1..=65535"));
    }

    #[test]
    fn port_must_be_numeric() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.expose.port: eighty
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn rfc3339_validated() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.policy.paused_until: tomorrow
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("RFC 3339"));
    }

    #[test]
    fn url_validated() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.hooks.post_deploy: not-a-url
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("URL"));
    }

    #[test]
    fn non_isengard_labels_pass_through() {
        let d = doc(r#"
services:
  web:
    labels:
      com.example.team: platform
      traefik.enable: "true"
"#);
        assert!(diagnose(&d).is_empty());
    }

    #[test]
    fn named_expose_rules_validate() {
        let d = doc(r#"
services:
  web:
    labels:
      isengard.expose.web: plex.vallee.casa
      isengard.expose.web.port: "32400"
      isengard.expose.api.tls: badmode
"#);
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("expected one of"));
    }

    #[test]
    fn parse_error_emits_single_diagnostic() {
        let d = doc("services:\n  web: { broken\n");
        let diags = diagnose(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("failed to parse"));
    }

    #[test]
    fn non_compose_document_returns_empty() {
        let d = Document {
            text: "broken".into(),
            kind: DocKind::Other,
            version: 1,
        };
        assert!(diagnose(&d).is_empty());
    }
}
