//! Build script: bake the real release version into the binary.
//!
//! Why this exists: the workspace `Cargo.toml` pins `version = "0.1.0-alpha"`
//! and never bumps. We tag `v0.5.x` releases in git but the binary built
//! from those tags still reports `0.1.0-alpha` via `env!("CARGO_PKG_VERSION")`.
//! That breaks `isd update --check` (always claims a newer release is
//! available) and leaves operators unable to tell which binary they have.
//!
//! Resolution priority, first match wins:
//!   1. `ISENGARD_RELEASE_VERSION` env (CI sets this on tagged release builds).
//!   2. `GITHUB_REF_NAME` env starting with `v` (also set by GitHub Actions
//!      on tag pushes, used as a fallback so a tag build that forgets the
//!      explicit env var still gets the right version).
//!   3. `git describe --tags --always --dirty` from the workspace root.
//!      Produces `v0.5.2` on a tag, `v0.5.2-3-gabc1234` on commits past it,
//!      `abc1234` when there's no reachable tag. Dev builds land here.
//!   4. `CARGO_PKG_VERSION` (the static `0.1.0-alpha`) as the last-ditch
//!      fallback when `git` is missing (e.g. building from a tarball).
//!
//! The resolved value is exported as `ISENGARD_BUILD_VERSION`; source code
//! reads it via `env!("ISENGARD_BUILD_VERSION")`.
//!
//! Kept as a per-crate copy of the same logic in `crates/isengard/build.rs`
//! on purpose: a shared `isengard-version` crate would have to set the env
//! at its own `build.rs` time, which `env!` in `isd`'s source can't see
//! (rustc-env scopes to the build script's package). Two small build.rs
//! files are simpler than a third crate that exists to dodge that scope.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=ISENGARD_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    if let Some(workspace_root) = workspace_root() {
        let git_dir = workspace_root.join(".git");
        if git_dir.is_dir() {
            println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
            println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join("packed-refs").display()
            );
        } else if git_dir.is_file() {
            println!("cargo:rerun-if-changed={}", git_dir.display());
        }
    }

    let version = resolve_version();
    println!("cargo:rustc-env=ISENGARD_BUILD_VERSION={version}");
}

fn resolve_version() -> String {
    if let Ok(v) = std::env::var("ISENGARD_RELEASE_VERSION") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(v) = std::env::var("GITHUB_REF_NAME") {
        let trimmed = v.trim();
        if trimmed.starts_with('v') {
            return trimmed.to_string();
        }
    }
    if let Some(v) = git_describe() {
        return v;
    }
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0-unknown".to_string())
}

fn git_describe() -> Option<String> {
    let workspace = workspace_root()?;
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(&workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn workspace_root() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let crate_dir = PathBuf::from(manifest_dir);
    crate_dir.parent()?.parent().map(PathBuf::from)
}
