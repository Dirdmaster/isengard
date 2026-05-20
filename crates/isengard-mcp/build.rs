//! Build-time helper for `isengard-mcp`.
//!
//! `embedded::API_DOCS` used to call `include_dir!` on the entire
//! workspace `crates/` directory, baking every Rust source file into
//! the binary so the macro could find the per-crate `docs/` subtrees.
//! That ballooned the binary's data section with `*.rs`, `Cargo.toml`,
//! and unrelated artifacts the MCP server filters out at runtime.
//!
//! This script materializes a `docs/`-only mirror under
//! `$OUT_DIR/api_docs/` at compile time. The mirror preserves the
//! `<crate>/docs/<file>` layout `resources.rs` walks, plus the nested
//! `isengard-plugins/<plugin>/docs/<file>` layout, so the runtime
//! consumer needs no change. `embedded.rs` then points `include_dir!`
//! at the mirror, and the binary embeds only markdown.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_crates = manifest_dir
        .parent()
        .expect("isengard-mcp is nested at least one level deep")
        .to_path_buf();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let mirror_root = out_dir.join("api_docs");

    // Wipe and recreate so a removed docs file doesn't linger.
    if mirror_root.exists() {
        fs::remove_dir_all(&mirror_root).expect("wipe stale mirror");
    }
    fs::create_dir_all(&mirror_root).expect("create mirror root");

    mirror_docs_recursive(&workspace_crates, &workspace_crates, &mirror_root);

    println!("cargo:rerun-if-changed=build.rs");
    // Coarse-grained: any change inside crates/ should re-run the
    // build. Cargo dedupes against file mtimes so the actual rebuild
    // only fires when docs files change.
    println!("cargo:rerun-if-changed={}", workspace_crates.display());
}

/// Walk `dir` for any `docs/` subdirectory and copy its contents into
/// `mirror_root` at the same relative path under the workspace
/// `crates/` root. Recurses into subdirectories so nested plugin
/// crates (`crates/isengard-plugins/<plugin>/docs/`) are mirrored
/// faithfully.
fn mirror_docs_recursive(crates_root: &Path, dir: &Path, mirror_root: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if file_name == "docs" {
            let rel = path
                .strip_prefix(crates_root)
                .expect("docs path is under crates_root");
            let dst = mirror_root.join(rel);
            fs::create_dir_all(&dst).expect("create mirror dir");
            copy_tree(&path, &dst);
            continue;
        }
        // Skip target/, .git/, hidden dirs.
        if file_name.starts_with('.') || file_name == "target" {
            continue;
        }
        mirror_docs_recursive(crates_root, &path, mirror_root);
    }
}

/// Recursive directory copy. Replaces nothing; assumes `dst` is
/// freshly created.
fn copy_tree(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).expect("read source dir").flatten() {
        let path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if path.is_dir() {
            fs::create_dir_all(&dst_path).expect("create dst subdir");
            copy_tree(&path, &dst_path);
        } else {
            fs::copy(&path, &dst_path).expect("copy file");
        }
    }
}
