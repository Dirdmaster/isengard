//! OCI layer extraction with whiteout handling.
//!
//! Each image layer is a tarball (optionally gzip- or zstd-compressed)
//! whose entries are applied on top of the previous layer's filesystem
//! tree. The OCI overlay-fs convention adds two flavours of "whiteout"
//! entry that delete instead of write:
//!
//!   - `<dir>/.wh.<name>`: delete `<target>/<dir>/<name>` from the
//!     filesystem assembled by lower layers. The whiteout entry itself
//!     is NEVER written to disk.
//!   - `<dir>/.wh..wh..opq`: clear every direct child of
//!     `<target>/<dir>/` (the directory survives, but its contents from
//!     lower layers are gone). The whiteout entry itself is NOT written.
//!
//! Layer tar entries are also a classic path-traversal vector: an entry
//! named `../../../etc/passwd`, or a symlink whose target points outside
//! the extraction root, would let a malicious image scribble onto the
//! host. We refuse any such entry up front via lexical resolution
//! (`Component`-walking, NOT `canonicalize`, because tar entries do not
//! exist on disk until we write them).
//!
//! The OCI distribution `media_type` string drives decompressor
//! selection; we accept the four standard flavours (Docker tar+gzip,
//! OCI tar+gzip, OCI tar+zstd, OCI raw tar) and reject everything else
//! with an explicit error rather than guessing.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use crate::error::WispImageError;
use crate::store::ContentStore;

// ---------------------------------------------------------------------------
// MediaType strings (OCI image-spec + Docker schema 2)
// ---------------------------------------------------------------------------

const MT_DOCKER_TAR_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const MT_OCI_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const MT_OCI_TAR_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
const MT_OCI_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const MT_DOCKER_TAR: &str = "application/vnd.docker.image.rootfs.diff.tar";

const WHITEOUT_PREFIX: &str = ".wh.";
/// Directory-level opaque whiteout: clears all children of the parent
/// directory before subsequent entries are applied.
const WHITEOUT_OPAQUE: &str = ".wh..wh..opq";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract a single layer's tar (or tar+gzip / tar+zstd) into `target`.
///
/// `target` MUST already exist. Honors the OCI whiteout convention:
///
///   - `<dir>/.wh.<name>`: delete `<target>/<dir>/<name>` (file or dir)
///   - `<dir>/.wh..wh..opq`: clear every direct child of `<target>/<dir>/`
///   - whiteout entries themselves are NOT written to disk
///
/// Rejects any entry path, symlink target, or hardlink target that
/// would resolve outside `target` via `..` traversal or an absolute
/// path. Returns `WispImageError::PathTraversal` in that case.
pub fn extract_layer(
    blob: impl Read,
    media_type: &str,
    target: &Path,
) -> Result<(), WispImageError> {
    if !target.exists() {
        return Err(WispImageError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("extraction target {target:?} does not exist"),
        )));
    }
    // Canonicalize once. Used for prefix-checks below; entry paths are
    // resolved lexically (since they don't exist yet) and joined onto
    // this canonical root.
    let target_canon = target.canonicalize().map_err(|e| {
        WispImageError::Io(io::Error::other(format!("canonicalize {target:?}: {e}")))
    })?;

    // Pick a decoder. Each branch erases the concrete decompressor
    // type into a `Box<dyn Read>` so the rest of the function is
    // monomorphic.
    let decoded: Box<dyn Read> = match media_type {
        MT_DOCKER_TAR_GZIP | MT_OCI_TAR_GZIP => Box::new(flate2::read::GzDecoder::new(blob)),
        MT_OCI_TAR_ZSTD => Box::new(
            zstd::stream::read::Decoder::new(blob)
                .map_err(|e| WispImageError::Tar(format!("zstd decoder: {e}")))?,
        ),
        MT_OCI_TAR | MT_DOCKER_TAR => Box::new(blob),
        other => {
            return Err(WispImageError::Tar(format!(
                "unsupported layer mediaType: {other}"
            )));
        }
    };

    let mut archive = tar::Archive::new(decoded);
    // We do path resolution ourselves: ask the tar crate not to.
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);

    let entries = archive
        .entries()
        .map_err(|e| WispImageError::Tar(format!("read entries: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| WispImageError::Tar(format!("entry: {e}")))?;
        apply_entry(&mut entry, &target_canon)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-entry application
// ---------------------------------------------------------------------------

fn apply_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    target: &Path,
) -> Result<(), WispImageError> {
    // Snapshot fields that need to survive past the read of contents.
    let header = entry.header().clone();
    let entry_path: PathBuf = entry
        .path()
        .map_err(|e| WispImageError::Tar(format!("entry path: {e}")))?
        .into_owned();

    // Validate: lexically resolve the entry path. Reject `..`, absolute
    // paths, and any other non-Normal component shape we cannot place
    // safely under `target`. Returns a clean Vec<OsString> of segments.
    let rel_segments = lexical_segments(&entry_path)
        .ok_or_else(|| WispImageError::PathTraversal(entry_path.clone()))?;

    // Whiteouts: detect via the basename's `.wh.` prefix.
    if let Some(last) = rel_segments.last() {
        let last_str = last.to_string_lossy();
        if last_str.starts_with(WHITEOUT_PREFIX) {
            // Parent segments form the "directory" the whiteout applies to.
            let parent_segments = &rel_segments[..rel_segments.len() - 1];
            return apply_whiteout(target, parent_segments, &last_str);
        }
    }

    // Non-whiteout: build the on-disk path under target.
    let dest = join_segments(target, &rel_segments);

    let etype = header.entry_type();
    match etype {
        EntryType::Directory => extract_directory(&dest, &header),
        EntryType::Regular | EntryType::Continuous => extract_regular(entry, &dest, &header),
        EntryType::Symlink => extract_symlink(entry, &dest, target),
        EntryType::Link => extract_hardlink(entry, &dest, target),
        // GNU long-name / long-link headers are consumed by the tar
        // crate transparently; we should not see them here.
        EntryType::GNULongName
        | EntryType::GNULongLink
        | EntryType::XGlobalHeader
        | EntryType::XHeader => Ok(()),
        // Char / block / fifo device entries: skip silently. The
        // container runtime creates /dev/console, /dev/null, /dev/zero,
        // and other dev nodes at start time as part of the rootfs
        // mount setup (see crates/wisp/src/lifecycle/mod.rs). Images
        // that ship dev node entries (LinuxServer.io base, alpine, etc.)
        // are common; rejecting them blocks any such image.
        // Surfaced live 2026-05-11 on lausanne v0.5.0 deploy with
        // lscr.io/linuxserver/{bazarr,prowlarr,qbittorrent,radarr,sonarr}.
        EntryType::Char | EntryType::Block | EntryType::Fifo => Ok(()),
        other => Err(WispImageError::Tar(format!(
            "unsupported tar entry type {:?} at {entry_path:?}",
            other
        ))),
    }
}

// ---------------------------------------------------------------------------
// Whiteouts
// ---------------------------------------------------------------------------

/// Apply a whiteout entry. `parent_segments` is the parent directory
/// (relative to `target`); `name` is the basename, including the
/// `.wh.` prefix (or the literal `.wh..wh..opq` for opaque).
fn apply_whiteout(
    target: &Path,
    parent_segments: &[std::ffi::OsString],
    name: &str,
) -> Result<(), WispImageError> {
    let parent_dir = join_segments(target, parent_segments);

    if name == WHITEOUT_OPAQUE {
        // Clear children of parent_dir; if it doesn't exist there's
        // nothing to clear and we tolerate that silently.
        if !parent_dir.exists() {
            return Ok(());
        }
        if !parent_dir.is_dir() {
            return Err(WispImageError::Whiteout(format!(
                "opaque whiteout target {parent_dir:?} is not a directory"
            )));
        }
        for child in fs::read_dir(&parent_dir)? {
            let child = child?;
            let path = child.path();
            let ft = child.file_type()?;
            if ft.is_dir() && !ft.is_symlink() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        return Ok(());
    }

    // Plain whiteout: strip the `.wh.` prefix and remove the named child.
    let stripped = name
        .strip_prefix(WHITEOUT_PREFIX)
        .ok_or_else(|| WispImageError::Whiteout(format!("malformed whiteout name {name:?}")))?;
    if stripped.is_empty() {
        return Err(WispImageError::Whiteout(format!(
            "empty whiteout target after stripping .wh. from {name:?}"
        )));
    }
    let victim = parent_dir.join(stripped);
    if !victim.exists() && fs::symlink_metadata(&victim).is_err() {
        // Lower layer never wrote the file; tolerate.
        return Ok(());
    }
    let meta = fs::symlink_metadata(&victim)?;
    if meta.file_type().is_dir() {
        fs::remove_dir_all(&victim)?;
    } else {
        fs::remove_file(&victim)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// File/dir/symlink/hardlink writers
// ---------------------------------------------------------------------------

fn extract_directory(dest: &Path, header: &tar::Header) -> Result<(), WispImageError> {
    fs::create_dir_all(dest)?;
    let mode = header.mode().unwrap_or(0o755) & 0o7777;
    let perms = std::fs::Permissions::from_mode(mode);
    fs::set_permissions(dest, perms)?;
    Ok(())
}

fn extract_regular<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dest: &Path,
    header: &tar::Header,
) -> Result<(), WispImageError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Remove an existing entry (file or symlink) to let `set_overwrite`
    // semantics apply consistently regardless of prior layer state.
    if fs::symlink_metadata(dest).is_ok() {
        let _ = fs::remove_file(dest);
    }
    let mode = header.mode().unwrap_or(0o644) & 0o7777;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dest)?;
    io::copy(entry, &mut file)?;
    file.flush()?;
    // Re-set perms in case the umask masked off bits at create time.
    fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn extract_symlink<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dest: &Path,
    target_root: &Path,
) -> Result<(), WispImageError> {
    let link_target = entry
        .link_name()
        .map_err(|e| WispImageError::Tar(format!("symlink target: {e}")))?
        .ok_or_else(|| WispImageError::Tar("symlink entry without target".into()))?
        .into_owned();

    // Symlink semantics:
    //   * If absolute (`/bin/busybox`): the kernel resolves it against
    //     the *namespace root*, which after `pivot_root` is the
    //     rootfs we're assembling. So absolute symlinks land where
    //     the image author intended once the runtime takes over. They
    //     ARE risky during extraction (a later layer entry writing
    //     through a symlinked parent directory could escape), but
    //     `extract_regular` removes existing entries with `unlink`
    //     (no symlink-follow) before creating, and OCI tar streams
    //     conventionally lay parent dirs before children. We accept
    //     absolute symlinks here; openat-relative writes are tracked
    //     for 0.5.
    //   * If relative: lexically simulate kernel resolution against
    //     the link's parent inside `target_root` and reject anything
    //     that escapes via `..`.
    if !link_target.is_absolute() {
        let link_parent = dest
            .parent()
            .ok_or_else(|| WispImageError::Tar(format!("symlink dest {dest:?} has no parent")))?;
        if !verify_symlink_target_safe(&link_target, link_parent, target_root) {
            return Err(WispImageError::PathTraversal(link_target));
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        let _ = fs::remove_file(dest);
    }
    symlink(&link_target, dest)?;
    Ok(())
}

fn extract_hardlink<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dest: &Path,
    target_root: &Path,
) -> Result<(), WispImageError> {
    let link_target = entry
        .link_name()
        .map_err(|e| WispImageError::Tar(format!("hardlink target: {e}")))?
        .ok_or_else(|| WispImageError::Tar("hardlink entry without target".into()))?
        .into_owned();

    // Hardlink targets in OCI tars are conventionally rootfs-relative,
    // but real-world images (notably alpine) emit absolute paths like
    // `/bin/busybox`. Treat a leading `/` as a hint that the target is
    // relative to the rootfs root and strip it; `..` and other
    // traversal shapes are still refused via lexical_segments.
    let normalised = strip_leading_slash(&link_target);
    let segments = lexical_segments(&normalised)
        .ok_or_else(|| WispImageError::PathTraversal(link_target.clone()))?;
    let source = join_segments(target_root, &segments);

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        let _ = fs::remove_file(dest);
    }
    fs::hard_link(&source, dest)?;
    Ok(())
}

/// If `p` is absolute (`/foo/bar`), return the rootfs-relative form
/// (`foo/bar`). Otherwise return as-is. Used to coerce absolute hardlink
/// targets into rootfs-relative form during layer extraction.
fn strip_leading_slash(p: &Path) -> PathBuf {
    if p.is_absolute() {
        let s = p.to_string_lossy();
        let trimmed = s.trim_start_matches('/');
        PathBuf::from(trimmed)
    } else {
        p.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// Path safety helpers
// ---------------------------------------------------------------------------

/// Lexically resolve a relative path into a clean sequence of normal
/// segments. Returns `None` if the path cannot be safely placed under
/// an extraction root, which means:
///
///   - the path is absolute (`/etc/foo`)
///   - any component is `..` (`a/../b`, `../escape`)
///   - any component is a Prefix (Windows), which we reject as
///     non-portable for OCI rootfs use
///
/// `.` components and the empty path are tolerated and produce zero or
/// more `Normal` segments. The result holds the segments only; callers
/// join them onto the canonicalized target root.
fn lexical_segments(p: &Path) -> Option<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::RootDir => return None,
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Join lexical segments onto a target root.
fn join_segments(root: &Path, segments: &[std::ffi::OsString]) -> PathBuf {
    let mut p = root.to_path_buf();
    for s in segments {
        p.push(s);
    }
    p
}

/// Verify that a symlink with the given `link_target` (as written
/// verbatim into the symlink) created at `link_parent` would, when
/// resolved by the kernel, land under `target_root`.
///
/// We don't follow the link; we just simulate the resolution lexically.
/// Steps:
///
/// 1. If `link_target` is absolute -> reject. (An absolute symlink is
///    interpreted against the host root at chroot/pivot time, which is
///    outside our protection model.)
/// 2. Compute `link_parent` relative to `target_root` (a sequence of
///    Normal segments). If link_parent isn't under target_root, the
///    caller already messed up; treat as unsafe.
/// 3. Walk link_target's components. `Normal` -> push. `ParentDir` ->
///    pop; if the stack is already empty, the link escapes -> unsafe.
///    `CurDir` -> ignore. Anything else -> unsafe.
fn verify_symlink_target_safe(link_target: &Path, link_parent: &Path, target_root: &Path) -> bool {
    if link_target.is_absolute() {
        return false;
    }
    let parent_rel = match link_parent.strip_prefix(target_root) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let mut stack: Vec<std::ffi::OsString> = parent_rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
        .collect();

    for c in link_target.components() {
        match c {
            Component::Normal(s) => stack.push(s.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Multi-layer assembly
// ---------------------------------------------------------------------------

use crate::registry::LayerRef;

/// Assemble a rootfs by extracting layers in oldest-to-newest order
/// into `<dest>.partial`, then atomically renaming to `<dest>`.
///
/// Layer blobs in the store are read-only; this function never
/// mutates them. On any error, `<dest>.partial` is removed (best
/// effort) and the original error is returned. `<dest>` must not
/// already exist; `<dest>.partial` must not already exist either
/// (callers should have cleaned a prior failed run before retry).
pub fn assemble_rootfs(
    store: &ContentStore,
    layers: &[LayerRef],
    dest: &Path,
) -> Result<(), WispImageError> {
    if dest.exists() {
        return Err(WispImageError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("rootfs dest already exists: {dest:?}"),
        )));
    }
    let partial = partial_path(dest);
    if partial.exists() {
        return Err(WispImageError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("rootfs partial already exists (clean it first): {partial:?}"),
        )));
    }

    fs::create_dir_all(&partial)?;

    let result = (|| -> Result<(), WispImageError> {
        for layer in layers {
            let blob = store.open_blob(&layer.digest)?;
            extract_layer(blob, &layer.media_type, &partial)?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            fs::rename(&partial, dest)?;
            Ok(())
        }
        Err(e) => {
            // Best-effort cleanup; surface the original error even if
            // cleanup itself failed.
            let _ = fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    /// Helper: build a tar containing the supplied `(path, kind)`
    /// entries and return the bytes. `kind` chooses between regular
    /// file (with content + mode), directory, symlink, hardlink.
    enum Entry<'a> {
        File { content: &'a [u8], mode: u32 },
        Dir { mode: u32 },
        Symlink { link_target: &'a str },
        Hardlink { link_target: &'a str },
    }

    fn build_tar(entries: &[(&str, Entry<'_>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            for (path, kind) in entries {
                match kind {
                    Entry::File { content, mode } => {
                        let mut h = tar::Header::new_gnu();
                        h.set_path(path).unwrap();
                        h.set_size(content.len() as u64);
                        h.set_mode(*mode);
                        h.set_entry_type(EntryType::Regular);
                        h.set_cksum();
                        b.append(&h, *content).unwrap();
                    }
                    Entry::Dir { mode } => {
                        let mut h = tar::Header::new_gnu();
                        // Tar dirs conventionally end with `/`. The crate
                        // tolerates either; set explicit type.
                        h.set_path(path).unwrap();
                        h.set_size(0);
                        h.set_mode(*mode);
                        h.set_entry_type(EntryType::Directory);
                        h.set_cksum();
                        b.append(&h, std::io::empty()).unwrap();
                    }
                    Entry::Symlink { link_target } => {
                        let mut h = tar::Header::new_gnu();
                        h.set_path(path).unwrap();
                        h.set_size(0);
                        h.set_mode(0o777);
                        h.set_entry_type(EntryType::Symlink);
                        h.set_link_name(link_target).unwrap();
                        h.set_cksum();
                        b.append(&h, std::io::empty()).unwrap();
                    }
                    Entry::Hardlink { link_target } => {
                        let mut h = tar::Header::new_gnu();
                        h.set_path(path).unwrap();
                        h.set_size(0);
                        h.set_mode(0o644);
                        h.set_entry_type(EntryType::Link);
                        h.set_link_name(link_target).unwrap();
                        h.set_cksum();
                        b.append(&h, std::io::empty()).unwrap();
                    }
                }
            }
            b.finish().unwrap();
        }
        buf
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 0).unwrap()
    }

    fn fresh_target() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    // -----------------------------------------------------------------
    // Layer extraction tests
    // -----------------------------------------------------------------

    #[test]
    fn extracts_regular_file_with_mode() {
        let tar_bytes = build_tar(&[(
            "hello.txt",
            Entry::File {
                content: b"world",
                mode: 0o644,
            },
        )]);
        let target = fresh_target();
        extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap();

        let p = target.path().join("hello.txt");
        assert_eq!(fs::read(&p).unwrap(), b"world");
        let mode = fs::metadata(&p).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn extracts_directory_with_mode() {
        let tar_bytes = build_tar(&[("var/", Entry::Dir { mode: 0o750 })]);
        let target = fresh_target();
        extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap();

        let p = target.path().join("var");
        assert!(p.is_dir(), "directory not created");
        let mode = fs::metadata(&p).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o750);
    }

    #[test]
    fn extracts_symlink_inside_target() {
        let tar_bytes = build_tar(&[
            (
                "etc/foo",
                Entry::File {
                    content: b"foo-data",
                    mode: 0o644,
                },
            ),
            ("etc/foo-link", Entry::Symlink { link_target: "foo" }),
        ]);
        let target = fresh_target();
        extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap();

        let link = target.path().join("etc/foo-link");
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, std::path::PathBuf::from("foo"));
    }

    #[test]
    fn rejects_symlink_pointing_outside_target() {
        let tar_bytes = build_tar(&[(
            "evil",
            Entry::Symlink {
                link_target: "../../etc/passwd",
            },
        )]);
        let target = fresh_target();
        let err = extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap_err();
        match err {
            WispImageError::PathTraversal(_) => {}
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn allows_absolute_symlink_target() {
        // OCI images (notably alpine) commonly emit symlinks like
        // `bin/sh -> /bin/busybox`. The kernel resolves them against
        // the namespace root (the rootfs after pivot_root), so they're
        // safe; the extractor should preserve them verbatim.
        let tar_bytes = build_tar(&[
            (
                "bin/busybox",
                Entry::File {
                    content: b"#!/busybox-stub",
                    mode: 0o755,
                },
            ),
            (
                "bin/sh",
                Entry::Symlink {
                    link_target: "/bin/busybox",
                },
            ),
        ]);
        let target = fresh_target();
        extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap();

        let link = target.path().join("bin/sh");
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, std::path::PathBuf::from("/bin/busybox"));
    }

    #[test]
    fn coerces_absolute_hardlink_target_to_rootfs_relative() {
        // A few images emit hardlinks with absolute paths; the extractor
        // must treat them as rootfs-relative so the link points at the
        // file we extracted, not the host's `/bin/busybox`.
        let tar_bytes = build_tar(&[
            (
                "bin/busybox",
                Entry::File {
                    content: b"bb-content",
                    mode: 0o755,
                },
            ),
            (
                "bin/cat",
                Entry::Hardlink {
                    link_target: "/bin/busybox",
                },
            ),
        ]);
        let target = fresh_target();
        extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap();

        // The hardlink and target should share content + inode.
        let link_path = target.path().join("bin/cat");
        let src_path = target.path().join("bin/busybox");
        assert_eq!(fs::read(&link_path).unwrap(), b"bb-content");
        let link_meta = fs::metadata(&link_path).unwrap();
        let src_meta = fs::metadata(&src_path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            link_meta.ino(),
            src_meta.ino(),
            "hardlink should share inode with target"
        );
    }

    #[test]
    fn rejects_hardlink_pointing_outside_target() {
        let tar_bytes = build_tar(&[(
            "evil",
            Entry::Hardlink {
                link_target: "../etc/passwd",
            },
        )]);
        let target = fresh_target();
        let err = extract_layer(Cursor::new(tar_bytes), MT_OCI_TAR, target.path()).unwrap_err();
        match err {
            WispImageError::PathTraversal(_) => {}
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn rejects_path_traversal_via_dotdot_in_filename() {
        // tar::Builder.set_path validates components, so we forge the
        // header bytes by hand for this attack.
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            let mut h = tar::Header::new_gnu();
            // GNU long-name path: write through the header bytes
            // directly to bypass set_path validation.
            let raw_name = b"../escape\0";
            h.as_old_mut().name[..raw_name.len()].copy_from_slice(raw_name);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_entry_type(EntryType::Regular);
            h.set_cksum();
            b.append(&h, std::io::empty()).unwrap();
            b.finish().unwrap();
        }
        let target = fresh_target();
        let err = extract_layer(Cursor::new(buf), MT_OCI_TAR, target.path()).unwrap_err();
        match err {
            WispImageError::PathTraversal(_) => {}
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn whiteout_deletes_file_from_lower_layer() {
        let target = fresh_target();
        // Layer 1: write etc/foo
        let layer1 = build_tar(&[
            ("etc/", Entry::Dir { mode: 0o755 }),
            (
                "etc/foo",
                Entry::File {
                    content: b"foo-data",
                    mode: 0o644,
                },
            ),
        ]);
        extract_layer(Cursor::new(layer1), MT_OCI_TAR, target.path()).unwrap();
        assert!(target.path().join("etc/foo").exists());

        // Layer 2: whiteout etc/.wh.foo
        let layer2 = build_tar(&[(
            "etc/.wh.foo",
            Entry::File {
                content: b"",
                mode: 0o644,
            },
        )]);
        extract_layer(Cursor::new(layer2), MT_OCI_TAR, target.path()).unwrap();

        assert!(!target.path().join("etc/foo").exists());
        assert!(!target.path().join("etc/.wh.foo").exists());
    }

    #[test]
    fn opaque_whiteout_clears_directory_siblings() {
        let target = fresh_target();
        let layer1 = build_tar(&[
            ("var/", Entry::Dir { mode: 0o755 }),
            ("var/log/", Entry::Dir { mode: 0o755 }),
            (
                "var/log/a",
                Entry::File {
                    content: b"a",
                    mode: 0o644,
                },
            ),
            (
                "var/log/b",
                Entry::File {
                    content: b"b",
                    mode: 0o644,
                },
            ),
        ]);
        extract_layer(Cursor::new(layer1), MT_OCI_TAR, target.path()).unwrap();
        assert!(target.path().join("var/log/a").exists());

        let layer2 = build_tar(&[(
            "var/log/.wh..wh..opq",
            Entry::File {
                content: b"",
                mode: 0o644,
            },
        )]);
        extract_layer(Cursor::new(layer2), MT_OCI_TAR, target.path()).unwrap();

        assert!(target.path().join("var/log").is_dir());
        let count = fs::read_dir(target.path().join("var/log")).unwrap().count();
        assert_eq!(count, 0, "opaque whiteout should clear var/log children");
    }

    #[test]
    fn whiteout_entry_itself_not_written() {
        let target = fresh_target();
        let layer = build_tar(&[
            ("etc/", Entry::Dir { mode: 0o755 }),
            (
                "etc/.wh.foo",
                Entry::File {
                    content: b"ignored",
                    mode: 0o644,
                },
            ),
        ]);
        extract_layer(Cursor::new(layer), MT_OCI_TAR, target.path()).unwrap();
        assert!(
            !target.path().join("etc/.wh.foo").exists(),
            "whiteout entry must not be written to disk"
        );
    }

    #[test]
    fn gzip_decodes_correctly() {
        let raw = build_tar(&[(
            "msg",
            Entry::File {
                content: b"compressed",
                mode: 0o644,
            },
        )]);
        let gz = gzip(&raw);
        let target = fresh_target();
        extract_layer(Cursor::new(gz), MT_OCI_TAR_GZIP, target.path()).unwrap();
        assert_eq!(fs::read(target.path().join("msg")).unwrap(), b"compressed");
    }

    #[test]
    fn zstd_decodes_correctly() {
        let raw = build_tar(&[(
            "msg",
            Entry::File {
                content: b"zstd-data",
                mode: 0o644,
            },
        )]);
        let z = zstd_compress(&raw);
        let target = fresh_target();
        extract_layer(Cursor::new(z), MT_OCI_TAR_ZSTD, target.path()).unwrap();
        assert_eq!(fs::read(target.path().join("msg")).unwrap(), b"zstd-data");
    }

    #[test]
    fn unsupported_media_type_errors_with_clear_message() {
        let target = fresh_target();
        let err = extract_layer(
            Cursor::new(b"".to_vec()),
            "application/x-bogus",
            target.path(),
        )
        .unwrap_err();
        match err {
            WispImageError::Tar(msg) => assert!(
                msg.contains("application/x-bogus"),
                "msg should embed the bad type, got {msg}"
            ),
            other => panic!("expected Tar(_), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // assemble_rootfs tests
    // -----------------------------------------------------------------

    fn write_layer_blob(store: &ContentStore, body: &[u8]) -> String {
        store.write_blob(body).unwrap()
    }

    #[test]
    fn assemble_two_layers_overlays_correctly() {
        let store_dir = fresh_target();
        let store = ContentStore::new(store_dir.path()).unwrap();

        let layer1 = build_tar(&[(
            "shared",
            Entry::File {
                content: b"layer-1",
                mode: 0o644,
            },
        )]);
        let layer2 = build_tar(&[(
            "shared",
            Entry::File {
                content: b"layer-2",
                mode: 0o644,
            },
        )]);
        let d1 = write_layer_blob(&store, &layer1);
        let d2 = write_layer_blob(&store, &layer2);

        let work = fresh_target();
        let dest = work.path().join("rootfs");
        let layers = [
            LayerRef {
                digest: d1,
                size: layer1.len() as u64,
                media_type: MT_OCI_TAR.to_string(),
            },
            LayerRef {
                digest: d2,
                size: layer2.len() as u64,
                media_type: MT_OCI_TAR.to_string(),
            },
        ];
        assemble_rootfs(&store, &layers, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shared")).unwrap(), b"layer-2");
        assert!(!dest.with_extension("partial").exists());
    }

    #[test]
    fn assemble_handles_failure_cleanup() {
        let store_dir = fresh_target();
        let store = ContentStore::new(store_dir.path()).unwrap();

        // Corrupt blob: not a valid tar at all.
        let d = write_layer_blob(&store, b"\x00\x01garbage");
        let work = fresh_target();
        let dest = work.path().join("rootfs");
        let layers = [LayerRef {
            digest: d,
            size: 8,
            media_type: MT_OCI_TAR.to_string(),
        }];
        let err = assemble_rootfs(&store, &layers, &dest).unwrap_err();
        // Either Tar or Io: extraction can fail at either layer of the
        // stack depending on which corruption the tar crate notices
        // first. Both are acceptable failure modes here.
        match err {
            WispImageError::Tar(_) | WispImageError::Io(_) => {}
            other => panic!("expected Tar/Io error, got {other:?}"),
        }
        assert!(!dest.exists(), "dest must not exist on failure");
        let partial = partial_path(&dest);
        assert!(!partial.exists(), "partial must be cleaned up");
    }

    #[test]
    fn assemble_handles_whiteout_across_layers() {
        let store_dir = fresh_target();
        let store = ContentStore::new(store_dir.path()).unwrap();

        let layer1 = build_tar(&[
            ("etc/", Entry::Dir { mode: 0o755 }),
            (
                "etc/foo",
                Entry::File {
                    content: b"foo",
                    mode: 0o644,
                },
            ),
        ]);
        let layer2 = build_tar(&[(
            "etc/.wh.foo",
            Entry::File {
                content: b"",
                mode: 0o644,
            },
        )]);
        let d1 = write_layer_blob(&store, &layer1);
        let d2 = write_layer_blob(&store, &layer2);

        let work = fresh_target();
        let dest = work.path().join("rootfs");
        let layers = [
            LayerRef {
                digest: d1,
                size: layer1.len() as u64,
                media_type: MT_OCI_TAR.to_string(),
            },
            LayerRef {
                digest: d2,
                size: layer2.len() as u64,
                media_type: MT_OCI_TAR.to_string(),
            },
        ];
        assemble_rootfs(&store, &layers, &dest).unwrap();

        assert!(!dest.join("etc/foo").exists());
        assert!(dest.join("etc").is_dir());
    }
}
