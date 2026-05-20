//! YAML walker. Locates every `labels:` entry under a compose `services:`
//! mapping and reports each label's key, value, and span.
//!
//! Pure module: no LSP types, no I/O. Diagnostics build LSP `Diagnostic`
//! values on top of the [`LabelEntry`] list this module returns.

use marked_yaml::types::Span;
use marked_yaml::{LoadError, Node, parse_yaml};

/// One `labels:` entry the walker found.
#[derive(Debug, Clone)]
pub struct LabelEntry {
    /// The label key as written (e.g. `isengard.policy.gate`).
    pub key: String,
    /// The label value as a string. Compose accepts both `key: value` and
    /// `- key=value`; we normalise both into this string.
    pub value: String,
    /// Source range of the key node. Used for the diagnostic underline.
    pub key_span: Span,
    /// Source range of the value node. Used when the value (not the key)
    /// is the offending part.
    pub value_span: Span,
    /// Compose service the entry belongs to. Used by diagnostics to name
    /// the offending service in messages.
    pub service: String,
}

/// Parse `text` and return every label found under
/// `services.<svc>.labels`.
///
/// Returns `Ok(vec![])` for compose docs with no `services:` block. Returns
/// `Err` only when the YAML itself fails to parse; bad shapes (e.g.
/// `labels:` being a string instead of a map) are silently skipped so the
/// caller can keep emitting other diagnostics on the doc.
///
/// # Errors
///
/// Returns the underlying [`LoadError`] when the YAML is not well-formed.
///
/// # Examples
///
/// ```
/// use isengard_lsp::yaml::find_labels;
///
/// let doc = r#"
/// services:
///   web:
///     image: nginx
///     labels:
///       isengard.enable: "true"
///       isengard.policy.gate: approval
/// "#;
/// let entries = find_labels(doc).unwrap();
/// assert_eq!(entries.len(), 2);
/// assert!(entries.iter().any(|e| e.key == "isengard.policy.gate"));
/// ```
pub fn find_labels(text: &str) -> Result<Vec<LabelEntry>, LoadError> {
    let root = parse_yaml(0, text)?;
    let mut out = Vec::new();
    let Some(root_map) = root.as_mapping() else {
        return Ok(out);
    };
    let Some(services) = root_map.get_mapping("services") else {
        return Ok(out);
    };
    for (svc_key, svc_node) in services.iter() {
        let svc_name = svc_key.as_str().to_string();
        let Some(svc_map) = svc_node.as_mapping() else {
            continue;
        };
        let Some(labels_node) = svc_map.get_node("labels") else {
            continue;
        };
        collect_labels(labels_node, &svc_name, &mut out);
    }
    Ok(out)
}

/// Walk a compose `labels:` node. Accepts both shapes:
///
/// - mapping (`labels: { foo: bar }` or block form).
/// - sequence of `key=value` strings (`labels: ["foo=bar"]`).
fn collect_labels(node: &Node, service: &str, out: &mut Vec<LabelEntry>) {
    match node {
        Node::Mapping(map) => {
            for (k, v) in map.iter() {
                let Some(scalar) = v.as_scalar() else {
                    // Skip non-scalar values (compose accepts only strings here).
                    continue;
                };
                out.push(LabelEntry {
                    key: k.as_str().to_string(),
                    value: scalar.as_str().to_string(),
                    key_span: *k.span(),
                    value_span: *scalar.span(),
                    service: service.to_string(),
                });
            }
        }
        Node::Sequence(seq) => {
            for item in seq.iter() {
                let Some(scalar) = item.as_scalar() else {
                    continue;
                };
                let raw = scalar.as_str();
                let Some((k, v)) = raw.split_once('=') else {
                    continue;
                };
                out.push(LabelEntry {
                    key: k.to_string(),
                    value: v.to_string(),
                    // List-shape labels share the scalar's span for both
                    // key and value: the YAML parser sees one node. The
                    // diagnostic underlines the whole `key=value` cell.
                    key_span: *scalar.span(),
                    value_span: *scalar.span(),
                    service: service.to_string(),
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_labels_under_each_service() {
        let doc = r#"
services:
  web:
    image: nginx
    labels:
      isengard.enable: "true"
      isengard.expose: plex.vallee.casa
  db:
    image: postgres
    labels:
      isengard.policy.gate: approval
"#;
        let entries = find_labels(doc).unwrap();
        assert_eq!(entries.len(), 3);
        let services: Vec<_> = entries.iter().map(|e| e.service.as_str()).collect();
        assert!(services.contains(&"web"));
        assert!(services.contains(&"db"));
    }

    #[test]
    fn supports_list_form_labels() {
        let doc = r#"
services:
  app:
    image: alpine
    labels:
      - "isengard.enable=true"
      - "isengard.expose.port=8080"
"#;
        let entries = find_labels(doc).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].key, "isengard.expose.port");
        assert_eq!(entries[1].value, "8080");
    }

    #[test]
    fn empty_compose_returns_empty() {
        let entries = find_labels("services: {}\n").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn missing_services_is_not_an_error() {
        let entries = find_labels("version: \"3.9\"\n").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn malformed_yaml_returns_error() {
        let bad = "services:\n  web: { broken\n";
        assert!(find_labels(bad).is_err());
    }

    #[test]
    fn records_line_column_for_keys() {
        let doc = "services:\n  web:\n    labels:\n      isengard.enable: \"true\"\n";
        let entries = find_labels(doc).unwrap();
        assert_eq!(entries.len(), 1);
        let start = entries[0].key_span.start().expect("key start marker");
        assert_eq!(start.line(), 4);
        // marked-yaml 1-indexes columns; the key sits at column 7 after
        // six spaces of indent.
        assert_eq!(start.column(), 7);
    }
}
