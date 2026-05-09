//! Content-addressable store for image blobs, manifest digests, and
//! per-bundle layer references.
//!
//! Atomic writes via `tempfile::NamedTempFile::persist`. Cross-process
//! coordination via an exclusive flock on `<root>/.lock` held during
//! writes. Reads are lock-free; they tolerate races by relying on the
//! atomic-rename invariant (a reader either sees the old file or the
//! new file, never a torn intermediate).

pub mod layout;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::error::WispImageError;
use crate::reference::ImageRef;

/// Result of a `gc` pass. `removed` is the list of blob digests that
/// were deleted; `kept` is the count of blobs still present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub removed: Vec<String>,
    pub kept: usize,
}

/// On-disk content store. Cheap to clone (root path only); all state
/// lives in the filesystem. Construct once via `ContentStore::new`.
#[derive(Debug, Clone)]
pub struct ContentStore {
    root: PathBuf,
}

impl ContentStore {
    /// Open or create a store rooted at `root`. Creates `blobs/sha256`,
    /// `index`, and `refs` subdirectories. Idempotent.
    pub fn new(root: &Path) -> Result<Self, WispImageError> {
        fs::create_dir_all(layout::sha256_dir(root))?;
        fs::create_dir_all(layout::index_root(root))?;
        fs::create_dir_all(layout::refs_root(root))?;
        // Touch the lock file so we can flock it on demand.
        let lock_file = layout::lock_path(root);
        if !lock_file.exists() {
            File::create(&lock_file)?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Root directory of the store. Mostly useful for tests.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path where the blob with the given digest lives (whether or not
    /// it currently exists). Errors if the digest's algorithm isn't
    /// `sha256` (Phase 0.2 supports sha256 only).
    pub fn blob_path(&self, digest: &str) -> Result<PathBuf, WispImageError> {
        let hex = layout::parse_sha256(digest).ok_or_else(|| {
            WispImageError::Parse(format!(
                "unsupported digest algorithm in {digest:?} (only sha256 in 0.2)"
            ))
        })?;
        Ok(layout::blob_path(&self.root, hex))
    }

    /// Hash `content`, write it atomically to the blob path, return
    /// the canonical `"sha256:<hex>"` digest. Idempotent: writing the
    /// same content twice keeps a single blob and does not error.
    pub fn write_blob(&self, content: &[u8]) -> Result<String, WispImageError> {
        let _guard = self.lock_exclusive()?;

        let mut hasher = Sha256::new();
        hasher.update(content);
        let hex = hex::encode(hasher.finalize());
        let digest = format!("sha256:{hex}");
        let target = layout::blob_path(&self.root, &hex);

        if target.exists() {
            return Ok(digest);
        }

        let dir = layout::sha256_dir(&self.root);
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(content)?;
        tmp.flush()?;
        // `persist` is atomic on Unix (rename(2)) and Windows
        // (MoveFileEx with REPLACE_EXISTING).
        tmp.persist(&target)
            .map_err(|e| WispImageError::Io(e.error))?;
        Ok(digest)
    }

    /// Streaming variant: hash while copying from `reader` to a
    /// tempfile in the blob directory. If `expected` is provided, the
    /// computed digest must match or the tempfile is dropped and a
    /// `Parse` error is returned. Returns the canonical digest on
    /// success. Idempotent (matches `write_blob`).
    pub fn write_blob_streaming<R: Read>(
        &self,
        mut reader: R,
        expected: Option<&str>,
    ) -> Result<String, WispImageError> {
        let _guard = self.lock_exclusive()?;

        let dir = layout::sha256_dir(&self.root);
        // Hold the tempfile until either persist or explicit drop on
        // mismatch. NamedTempFile cleans up on drop automatically.
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n])?;
        }
        tmp.flush()?;

        let hex = hex::encode(hasher.finalize());
        let digest = format!("sha256:{hex}");

        if let Some(want) = expected {
            if want != digest {
                // tmp drops here, removing the file from disk.
                return Err(WispImageError::Parse(format!(
                    "digest mismatch: expected {want}, got {digest}"
                )));
            }
        }

        let target = layout::blob_path(&self.root, &hex);
        if target.exists() {
            // Identical content already cached; drop the tempfile.
            return Ok(digest);
        }
        tmp.persist(&target)
            .map_err(|e| WispImageError::Io(e.error))?;
        Ok(digest)
    }

    /// Read an entire blob into memory. For large layer blobs callers
    /// should prefer `open_blob` to stream.
    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>, WispImageError> {
        let path = self.blob_path(digest)?;
        if !path.exists() {
            return Err(WispImageError::NotFound(digest.to_string()));
        }
        Ok(fs::read(path)?)
    }

    /// Open a blob for streaming reads.
    pub fn open_blob(&self, digest: &str) -> Result<File, WispImageError> {
        let path = self.blob_path(digest)?;
        if !path.exists() {
            return Err(WispImageError::NotFound(digest.to_string()));
        }
        Ok(File::open(path)?)
    }

    /// True iff a blob with the given digest is cached locally.
    pub fn has_blob(&self, digest: &str) -> bool {
        match self.blob_path(digest) {
            Ok(p) => p.exists(),
            Err(_) => false,
        }
    }

    /// Atomic tag write. Records `manifest_digest` (a `"sha256:..."`
    /// string) at `<root>/index/<registry>/<repo>/tag/<tag>`. Creates
    /// any intermediate directories.
    pub fn put_tag(
        &self,
        registry: &str,
        repo: &str,
        tag: &str,
        manifest_digest: &str,
    ) -> Result<(), WispImageError> {
        let _guard = self.lock_exclusive()?;
        let target = layout::tag_path(&self.root, registry, repo, tag);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let parent = target.parent().expect("tag path always has a parent");
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(manifest_digest.as_bytes())?;
        tmp.flush()?;
        tmp.persist(&target)
            .map_err(|e| WispImageError::Io(e.error))?;
        Ok(())
    }

    /// Read a tag's manifest digest, or `None` if no such tag is
    /// recorded. Returns `None` on any I/O error rather than
    /// propagating, matching the "lookup is best-effort" contract.
    pub fn lookup_tag(&self, registry: &str, repo: &str, tag: &str) -> Option<String> {
        let path = layout::tag_path(&self.root, registry, repo, tag);
        let bytes = fs::read(path).ok()?;
        let s = String::from_utf8(bytes).ok()?;
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// List every recorded `(ImageRef, manifest_digest)` pair by
    /// walking the index tree. Reconstructs the `ImageRef` from the
    /// directory layout (registry / repo segments / `tag` / tag-name).
    pub fn list_images(&self) -> Result<Vec<(ImageRef, String)>, WispImageError> {
        let mut out = Vec::new();
        let index_root = layout::index_root(&self.root);
        if !index_root.exists() {
            return Ok(out);
        }
        for registry_entry in fs::read_dir(&index_root)? {
            let registry_entry = registry_entry?;
            if !registry_entry.file_type()?.is_dir() {
                continue;
            }
            let registry = registry_entry.file_name().to_string_lossy().to_string();
            // Walk each registry sub-tree; whenever we see a directory
            // named `tag`, treat its parent as the repo path.
            walk_for_tags(&registry_entry.path(), &registry, &mut Vec::new(), &mut out)?;
        }
        Ok(out)
    }

    /// Record that `bundle_id` references the given layer digests.
    /// Overwrites any prior reference for the same bundle_id.
    pub fn add_ref(&self, bundle_id: &str, layer_digests: &[String]) -> Result<(), WispImageError> {
        let _guard = self.lock_exclusive()?;
        let dir = layout::ref_dir(&self.root, bundle_id);
        fs::create_dir_all(&dir)?;
        let target = layout::ref_layers_path(&self.root, bundle_id);
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        for d in layer_digests {
            writeln!(tmp, "{d}")?;
        }
        tmp.flush()?;
        tmp.persist(&target)
            .map_err(|e| WispImageError::Io(e.error))?;
        Ok(())
    }

    /// Remove the reference for `bundle_id`. Tolerates missing dirs
    /// (no-op).
    pub fn drop_ref(&self, bundle_id: &str) -> Result<(), WispImageError> {
        let _guard = self.lock_exclusive()?;
        let dir = layout::ref_dir(&self.root, bundle_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// Union of every layer digest referenced by every bundle.
    pub fn referenced_layers(&self) -> Result<HashSet<String>, WispImageError> {
        let mut out = HashSet::new();
        let refs_root = layout::refs_root(&self.root);
        if !refs_root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&refs_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let layers_file = entry.path().join("layers");
            if !layers_file.exists() {
                continue;
            }
            let bytes = fs::read(&layers_file)?;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let s = std::str::from_utf8(line)
                    .map_err(|e| WispImageError::Parse(format!("non-utf8 in layers file: {e}")))?
                    .trim();
                if !s.is_empty() {
                    out.insert(s.to_string());
                }
            }
        }
        Ok(out)
    }

    /// Drop every blob that is neither referenced by a bundle nor
    /// listed as a manifest digest in the index. Returns counts.
    pub fn gc(&self) -> Result<GcReport, WispImageError> {
        let _guard = self.lock_exclusive()?;
        let mut keep = self.referenced_layers()?;
        // Manifests pointed at by the index must also survive: the
        // image is "in the catalog" even if no bundle currently runs.
        for (_r, manifest_digest) in self.list_images()? {
            keep.insert(manifest_digest);
        }

        let blobs_dir = layout::sha256_dir(&self.root);
        let mut removed = Vec::new();
        let mut kept = 0usize;
        if !blobs_dir.exists() {
            return Ok(GcReport { removed, kept });
        }
        for entry in fs::read_dir(&blobs_dir)? {
            let entry = entry?;
            // Skip residual tempfiles from interrupted writes.
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // tempfile::NamedTempFile names start with `.tmp`. Hex blob
            // names are 64 chars of [0-9a-f]; skip anything that
            // doesn't look like a hex digest so we don't accidentally
            // delete the lock file or a tempfile.
            if !looks_like_sha256_hex(&name_str) {
                continue;
            }
            let digest = format!("sha256:{name_str}");
            if keep.contains(&digest) {
                kept += 1;
            } else {
                fs::remove_file(entry.path())?;
                removed.push(digest);
            }
        }
        Ok(GcReport { removed, kept })
    }

    fn lock_exclusive(&self) -> Result<LockGuard, WispImageError> {
        let path = layout::lock_path(&self.root);
        // `OpenOptions::create_new` would race; `.lock` is created in
        // `new()`, so plain open is fine here.
        let file = File::open(&path)?;
        FileExt::lock_exclusive(&file)?;
        Ok(LockGuard { file })
    }
}

/// RAII wrapper that releases the file lock on drop. The lock is
/// process-scoped; if the holding process dies, the kernel releases
/// the flock automatically.
struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort unlock; if it fails, the file close will release
        // it anyway. We deliberately ignore the result to keep `Drop`
        // panic-free.
        let _ = FileExt::unlock(&self.file);
    }
}

fn looks_like_sha256_hex(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Recursive helper for `list_images`. `accum` is the current repo
/// segment stack; when a `tag` directory is encountered, every file
/// inside contributes one `(ImageRef, manifest_digest)` entry.
fn walk_for_tags(
    dir: &Path,
    registry: &str,
    accum: &mut Vec<String>,
    out: &mut Vec<(ImageRef, String)>,
) -> Result<(), WispImageError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == layout::TAG_DIR && !accum.is_empty() {
            // accum is the repo (joined by `/`).
            let repo = accum.join("/");
            for tag_entry in fs::read_dir(entry.path())? {
                let tag_entry = tag_entry?;
                if !tag_entry.file_type()?.is_file() {
                    continue;
                }
                let tag_name = tag_entry.file_name().to_string_lossy().to_string();
                let bytes = fs::read(tag_entry.path())?;
                let manifest_digest = String::from_utf8(bytes)
                    .map_err(|e| WispImageError::Parse(format!("non-utf8 in tag file: {e}")))?
                    .trim()
                    .to_string();
                if manifest_digest.is_empty() {
                    continue;
                }
                let r = ImageRef {
                    registry: registry.to_string(),
                    repo: repo.clone(),
                    tag: Some(tag_name),
                    digest: None,
                };
                out.push((r, manifest_digest));
            }
        } else {
            accum.push(name);
            walk_for_tags(&entry.path(), registry, accum, out)?;
            accum.pop();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn store() -> (TempDir, ContentStore) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ContentStore::new(tmp.path()).expect("store::new");
        (tmp, store)
    }

    #[test]
    fn write_blob_returns_correct_digest() {
        let (_tmp, s) = store();
        // Pre-computed sha256("hello world") = b94d27b9934d3e08...
        let d = s.write_blob(b"hello world").unwrap();
        assert_eq!(
            d,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(s.has_blob(&d));
    }

    #[test]
    fn write_blob_is_idempotent() {
        let (_tmp, s) = store();
        let d1 = s.write_blob(b"abc").unwrap();
        let d2 = s.write_blob(b"abc").unwrap();
        assert_eq!(d1, d2);
        let blob_dir = layout::sha256_dir(s.root());
        let count = fs::read_dir(&blob_dir).unwrap().count();
        assert_eq!(count, 1, "idempotent write must keep a single file");
    }

    #[test]
    fn write_blob_streaming_verifies_expected() {
        let (_tmp, s) = store();
        let content = b"streaming content";
        let mut hasher = Sha256::new();
        hasher.update(content);
        let expected = format!("sha256:{}", hex::encode(hasher.finalize()));
        let got = s
            .write_blob_streaming(Cursor::new(content), Some(&expected))
            .unwrap();
        assert_eq!(got, expected);
        assert!(s.has_blob(&got));
    }

    #[test]
    fn write_blob_streaming_rejects_digest_mismatch() {
        let (_tmp, s) = store();
        let content = b"streaming content";
        let bogus = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let res = s.write_blob_streaming(Cursor::new(content), Some(bogus));
        assert!(res.is_err(), "expected mismatch error");
        // Tempfile must be cleaned up: blob dir should be empty.
        let blob_dir = layout::sha256_dir(s.root());
        let count = fs::read_dir(&blob_dir).unwrap().count();
        assert_eq!(count, 0);
    }

    #[test]
    fn put_tag_round_trips() {
        let (_tmp, s) = store();
        s.put_tag("docker.io", "library/alpine", "3.19", "sha256:deadbeef")
            .unwrap();
        let got = s.lookup_tag("docker.io", "library/alpine", "3.19");
        assert_eq!(got.as_deref(), Some("sha256:deadbeef"));
    }

    #[test]
    fn list_images_returns_persisted_entries() {
        let (_tmp, s) = store();
        s.put_tag("docker.io", "library/alpine", "3.19", "sha256:aaaa")
            .unwrap();
        s.put_tag("ghcr.io", "dirdmaster/foo", "v1", "sha256:bbbb")
            .unwrap();
        let mut listed = s.list_images().unwrap();
        listed.sort_by(|a, b| a.0.registry.cmp(&b.0.registry));
        assert_eq!(listed.len(), 2);

        let alpine = &listed[0];
        assert_eq!(alpine.0.registry, "docker.io");
        assert_eq!(alpine.0.repo, "library/alpine");
        assert_eq!(alpine.0.tag.as_deref(), Some("3.19"));
        assert_eq!(alpine.1, "sha256:aaaa");

        let foo = &listed[1];
        assert_eq!(foo.0.registry, "ghcr.io");
        assert_eq!(foo.0.repo, "dirdmaster/foo");
        assert_eq!(foo.0.tag.as_deref(), Some("v1"));
        assert_eq!(foo.1, "sha256:bbbb");
    }

    #[test]
    fn add_ref_then_referenced_layers_includes_them() {
        let (_tmp, s) = store();
        let layers = vec![
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string(),
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".to_string(),
        ];
        s.add_ref("bundle-a", &layers).unwrap();
        let referenced = s.referenced_layers().unwrap();
        for d in &layers {
            assert!(referenced.contains(d), "missing {d}");
        }
    }

    #[test]
    fn drop_ref_removes_dir() {
        let (_tmp, s) = store();
        s.add_ref(
            "bundle-x",
            &[
                "sha256:abc1234567890def0000000000000000000000000000000000000000000000abc"
                    .to_string(),
            ],
        )
        .unwrap();
        let dir = layout::ref_dir(s.root(), "bundle-x");
        assert!(dir.exists());
        s.drop_ref("bundle-x").unwrap();
        assert!(!dir.exists());
        // Tolerate dropping a missing ref.
        s.drop_ref("bundle-x").unwrap();
    }

    #[test]
    fn gc_drops_unreferenced_blobs_keeps_referenced() {
        let (_tmp, s) = store();
        let d1 = s.write_blob(b"layer-one").unwrap();
        let d2 = s.write_blob(b"layer-two").unwrap();
        let d3 = s.write_blob(b"layer-three-orphan").unwrap();

        s.add_ref("bundle-1", &[d1.clone(), d2.clone()]).unwrap();

        let report = s.gc().unwrap();
        assert_eq!(report.removed, vec![d3.clone()]);
        assert_eq!(report.kept, 2);

        assert!(s.has_blob(&d1));
        assert!(s.has_blob(&d2));
        assert!(!s.has_blob(&d3));
    }

    #[test]
    fn gc_keeps_manifest_digests_pointed_at_by_index() {
        let (_tmp, s) = store();
        // Write a "manifest" blob and tag it; it's not in any bundle's
        // layer list but should still survive GC.
        let manifest_digest = s.write_blob(b"{\"mediaType\":\"manifest\"}").unwrap();
        s.put_tag("docker.io", "library/alpine", "3.19", &manifest_digest)
            .unwrap();
        let orphan = s.write_blob(b"orphan-blob").unwrap();
        let report = s.gc().unwrap();
        assert!(s.has_blob(&manifest_digest));
        assert!(!s.has_blob(&orphan));
        assert_eq!(report.removed, vec![orphan]);
        assert_eq!(report.kept, 1);
    }
}
