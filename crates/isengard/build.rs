//! Build script: bake the real release version into the binary.
//!
//! Why this exists: the workspace `Cargo.toml` pins `version = "0.1.0-alpha"`
//! and never bumps. We tag `v0.5.x` releases in git but the binary built
//! from those tags still reports `0.1.0-alpha` via `env!("CARGO_PKG_VERSION")`.
//! That breaks `isengard update --check` (always claims a newer release is
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

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Re-run the build script whenever something that could change the
    // version moves: an env var override, the workspace HEAD, or any ref.
    // Without these the binary keeps reporting the stale version after a
    // `git checkout v0.5.3` because cargo thinks nothing changed.
    println!("cargo:rerun-if-env-changed=ISENGARD_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    if let Some(workspace_root) = workspace_root() {
        let git_dir = workspace_root.join(".git");
        // Cover both standard repos (`.git` is a directory) and
        // worktrees (`.git` is a file pointing at the gitdir).
        if git_dir.is_dir() {
            println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
            println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join("packed-refs").display()
            );
        } else if git_dir.is_file() {
            // Worktree: `.git` is a tiny pointer file. Watching it is
            // good enough; the actual refs live under the parent
            // repo's gitdir which cargo doesn't need to track here
            // because every worktree HEAD update touches this file.
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
    // Last-ditch fallback. The string still says "0.1.0-alpha", which
    // downstream callers detect via SemVer pre-release marker and treat
    // as "always allow update".
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

/// Walk up from this build script's directory (`crates/<name>/`) to the
/// workspace root. We need the workspace path for two things: telling
/// cargo which git files to watch, and running `git describe` from a
/// directory git recognises as a repo (a per-crate cwd works too, but
/// being explicit avoids surprises in sparse checkouts).
fn workspace_root() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let crate_dir = PathBuf::from(manifest_dir);
    // crates/<name>/  ->  crates/  ->  <workspace root>
    crate_dir.parent()?.parent().map(PathBuf::from)
}
