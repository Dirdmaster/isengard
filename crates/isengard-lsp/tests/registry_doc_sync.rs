//! Guard against drift between `crates/isengard-lsp/docs/LABELS.md` and the
//! executable [`REGISTRY`](isengard_lsp::registry::REGISTRY).
//!
//! The vault doc `docs/concepts/labels.md` is the operator-facing list;
//! the in-crate mirror at `docs/LABELS.md` is the machine-readable copy
//! this test cross-references. If somebody adds a label to one and not
//! the other, the test fails loud.

use std::collections::BTreeSet;

use isengard_lsp::registry::{LabelKey, REGISTRY};

/// Embed the in-crate mirror at compile time so `cargo test` does not
/// depend on a workdir layout.
const LABELS_DOC: &str = include_str!("../docs/LABELS.md");

#[test]
fn every_labels_md_row_has_a_registry_entry() {
    let documented = parse_labels_doc(LABELS_DOC);
    let registry: BTreeSet<String> = REGISTRY.iter().map(spec_label_string).collect();

    let missing: Vec<&String> = documented.difference(&registry).collect();
    let extra: Vec<&String> = registry.difference(&documented).collect();

    assert!(
        missing.is_empty(),
        "docs/LABELS.md lists labels with no REGISTRY entry: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "REGISTRY has labels missing from docs/LABELS.md: {extra:?}"
    );
}

#[test]
fn registry_size_matches_doc() {
    let documented = parse_labels_doc(LABELS_DOC);
    assert_eq!(
        REGISTRY.len(),
        documented.len(),
        "registry size diverged from docs/LABELS.md"
    );
}

/// Pull every label key out of the LABELS.md tables.
///
/// Rows look like `| \`isengard.policy.gate\` | Enum | ... |`. The first
/// backtick-fenced cell is the key. Pattern keys (`<name>`) are kept
/// verbatim so the registry's [`LabelKey::Pattern`] entries can be
/// stringified the same way. Only rows whose first cell starts with
/// `isengard.` or `io.isengard.` are kept; this drops the column-header
/// table rows used in the doc to describe the schema (`Label`, `Kind`,
/// `Values`).
fn parse_labels_doc(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("| `") {
            continue;
        }
        // Slice between the first pair of backticks on the line.
        let Some(start) = trimmed.find('`') else {
            continue;
        };
        let after = &trimmed[start + 1..];
        let Some(end) = after.find('`') else {
            continue;
        };
        let label = &after[..end];
        if !(label.starts_with("isengard.") || label.starts_with("io.isengard.")) {
            continue;
        }
        out.insert(label.to_string());
    }
    out
}

/// Stringify a [`LabelSpec.key`] back into the form `docs/LABELS.md` uses.
fn spec_label_string(spec: &'static isengard_lsp::registry::LabelSpec) -> String {
    match spec.key {
        LabelKey::Literal(s) => s.to_string(),
        LabelKey::Pattern { prefix, suffix } => match suffix {
            None => format!("{prefix}.<name>"),
            Some(s) => format!("{prefix}.<name>.{s}"),
        },
    }
}
