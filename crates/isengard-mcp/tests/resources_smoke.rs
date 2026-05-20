//! End-to-end smoke test for the embedded resource catalogue.
//!
//! Walks the real `docs/` and `crates/<crate>/docs/` trees baked
//! into the binary and verifies:
//!
//! - At least one operator guide surfaces under `isengard://docs/`.
//! - At least one per-crate API doc surfaces under `isengard://api/`.
//! - Every listed URI round-trips through `read_resource` and the
//!   returned body is non-empty.
//! - No `_dir.yml` Docus navigation files leak through.

use isengard_mcp::{ResourceUri, list_resources, read_resource};

#[test]
fn every_listed_resource_round_trips() {
    let entries = list_resources();
    assert!(
        !entries.is_empty(),
        "expected at least one embedded resource"
    );
    for entry in &entries {
        // URI parses.
        let parsed = ResourceUri::parse(&entry.uri);
        assert!(
            parsed.is_some(),
            "resource URI did not parse: {}",
            entry.uri,
        );
        // Read returns the same body.
        let body = read_resource(&entry.uri).expect("read_resource returns body");
        assert!(!body.is_empty(), "resource body is empty: {}", entry.uri);
        assert_eq!(body, entry.body);
    }
}

#[test]
fn separates_docs_from_api() {
    let entries = list_resources();
    let docs_count = entries
        .iter()
        .filter(|e| e.uri.starts_with("isengard://docs/"))
        .count();
    let api_count = entries
        .iter()
        .filter(|e| e.uri.starts_with("isengard://api/"))
        .count();
    assert!(docs_count >= 1, "expected operator docs in the catalogue");
    assert!(
        api_count >= 1,
        "expected per-crate API docs in the catalogue"
    );
}

#[test]
fn never_exposes_dir_yml_or_source() {
    let entries = list_resources();
    for entry in &entries {
        assert!(
            !entry.uri.contains("/_dir"),
            "Docus _dir.yml surfaced as a resource: {}",
            entry.uri,
        );
        assert!(
            !entry.uri.contains(".rs"),
            "Rust source surfaced as a resource: {}",
            entry.uri,
        );
        assert!(
            !entry.uri.contains("Cargo"),
            "Cargo manifest surfaced as a resource: {}",
            entry.uri,
        );
    }
}
