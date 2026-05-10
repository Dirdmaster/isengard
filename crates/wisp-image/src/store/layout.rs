//! Path conventions for the on-disk content store.
//!
//! The store is rooted at a single directory and uses a fixed sub-tree:
//!
//! ```text
//! <root>/
//!   .lock                      # file-locked during writes
//!   blobs/sha256/<hex>         # content-addressed payloads
//!   index/<registry>/<repo>/tag/<tag>   # text file: "sha256:<hex>"
//!   refs/<bundle-id>/layers    # one "sha256:<hex>" per line
//! ```
//!
//! All write paths flow through atomic `tempfile -> persist` so partial
//! writes are never visible to another reader.

use std::path::{Path, PathBuf};

pub const BLOBS_DIR: &str = "blobs";
pub const SHA256_DIR: &str = "sha256";
pub const INDEX_DIR: &str = "index";
pub const REFS_DIR: &str = "refs";
pub const TAG_DIR: &str = "tag";
pub const LOCK_FILE: &str = ".lock";

/// `<root>/blobs/sha256`
pub fn sha256_dir(root: &Path) -> PathBuf {
    root.join(BLOBS_DIR).join(SHA256_DIR)
}

/// `<root>/blobs/sha256/<hex>`
pub fn blob_path(root: &Path, hex: &str) -> PathBuf {
    sha256_dir(root).join(hex)
}

/// `<root>/index/<registry>/<repo>/tag/<tag>`. `repo` may contain `/`,
/// in which case the caller must mkdir parents before writing.
pub fn tag_path(root: &Path, registry: &str, repo: &str, tag: &str) -> PathBuf {
    root.join(INDEX_DIR)
        .join(registry)
        .join(repo)
        .join(TAG_DIR)
        .join(tag)
}

/// `<root>/refs/<bundle-id>`
pub fn ref_dir(root: &Path, bundle_id: &str) -> PathBuf {
    root.join(REFS_DIR).join(bundle_id)
}

/// `<root>/refs/<bundle-id>/layers`
pub fn ref_layers_path(root: &Path, bundle_id: &str) -> PathBuf {
    ref_dir(root, bundle_id).join("layers")
}

/// `<root>/.lock`
pub fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_FILE)
}

/// `<root>/index`
pub fn index_root(root: &Path) -> PathBuf {
    root.join(INDEX_DIR)
}

/// `<root>/refs`
pub fn refs_root(root: &Path) -> PathBuf {
    root.join(REFS_DIR)
}

/// Strip the `sha256:` prefix from a digest string. Returns the hex
/// payload as a `&str` borrowed from the input. Phase 0.2 supports
/// sha256 only; other algorithms produce `None` and callers should
/// surface a `Parse` / `NotFound` error.
pub fn parse_sha256(digest: &str) -> Option<&str> {
    digest.strip_prefix("sha256:")
}
