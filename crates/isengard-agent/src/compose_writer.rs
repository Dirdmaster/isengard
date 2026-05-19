//! v0.3c compose import: persist generated `compose.yaml` to a stack's
//! managed directory under `/etc/isengard/stacks/<stack>/`.
//!
//! Three guarantees:
//! 1. **Atomic write**: write to `compose.yaml.tmp`, fsync, rename. Power
//!    loss never leaves a partial file.
//! 2. **Manual-edit detection**: refuse to overwrite a `compose.yaml`
//!    whose sha256 doesn't match the `last_written_sha256` recorded in
//!    `meta.toml`. Caller must opt in via `force = true` to clobber.
//! 3. **Sibling metadata**: `meta.toml` records `source`, `imported_at`,
//!    `last_written_sha256`, `host_id` so the agent (and the eventual
//!    `isd adopt`) can reason about provenance.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Outcome of `write_compose`. Lets the agent emit a useful event
/// (`compose.imported`, `compose.skipped_no_change`,
/// `compose.refused_manual_edit`) without inspecting strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `compose.yaml` was written (first time or refreshed).
    Written,
    /// File on disk already matches; nothing to do.
    Unchanged,
}

/// Returned when the caller didn't pass `force = true` and the on-disk
/// file's sha256 doesn't match the agent's recorded `last_written_sha256`.
/// The caller decides whether to retry with `force = true`.
#[derive(Debug)]
pub struct ManualEditDetected {
    pub path: PathBuf,
    pub on_disk_sha256: String,
    pub expected_sha256: String,
}

impl std::fmt::Display for ManualEditDetected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "manual edit detected at {}: on_disk_sha256 {} != expected {}",
            self.path.display(),
            self.on_disk_sha256,
            self.expected_sha256,
        )
    }
}

impl std::error::Error for ManualEditDetected {}

/// Sibling `meta.toml` record. `source = "imported"` for v0.3c; v0.3d will
/// flip it to `"managed"` once `isd adopt` swaps the stack to compose-as-truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeMeta {
    /// "imported" (v0.3c) or "managed" (v0.3d). Round-trips verbatim.
    pub source: String,
    /// RFC3339 timestamp of the last write.
    pub imported_at: String,
    /// sha256 hex of the YAML bytes the agent wrote.
    pub last_written_sha256: String,
    /// Host ULID. Cross-checked when the operator copies the directory
    /// between machines.
    pub host_id: String,
    /// v0.3d: sha256 of the YAML the reconciler last applied to the
    /// running containers. The watcher consults this to skip reconcile
    /// loops triggered by the agent's own writes (which already match
    /// the running state). Optional for back-compat with v0.3c
    /// `meta.toml` records that lack the field.
    #[serde(default)]
    pub last_applied_sha256: Option<String>,
}

/// Compute the sha256 hex of `s`.
pub fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Write `content` to `dir/compose.yaml`. See module docs for guarantees.
///
/// On success, also writes `dir/meta.toml` with the sha256 hash and
/// timestamp so the next import knows whether the on-disk file is the one
/// the agent wrote.
///
/// Returns:
/// - `Ok(WriteOutcome::Written)`: file was created or refreshed.
/// - `Ok(WriteOutcome::Unchanged)`: on-disk content already matches.
/// - `Err(ManualEditDetected)`: file exists with a different sha than the
///   agent's recorded one and `force` was false.
/// - `Err(other)`: I/O failure.
pub fn write_compose(
    dir: &Path,
    content: &str,
    host_id: &str,
    imported_at_rfc3339: &str,
    force: bool,
) -> Result<WriteOutcome, anyhow::Error> {
    let path = dir.join("compose.yaml");
    let meta_path = dir.join("meta.toml");

    let new_sha = sha256_hex(content);

    if path.exists() {
        let on_disk = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!("read existing compose.yaml at {}: {e}", path.display())
        })?;
        let on_disk_sha = sha256_hex(&on_disk);

        if on_disk_sha == new_sha {
            // Refresh meta.toml's timestamp anyway? No: a no-op write leaves
            // the prior `imported_at` intact, which is more honest. Just
            // signal Unchanged so the caller can skip the report-back.
            return Ok(WriteOutcome::Unchanged);
        }

        // Existing file disagrees. If we have a meta.toml, compare its
        // recorded sha against on_disk_sha to tell apart "stale agent
        // copy" from "user edited the file".
        let meta = read_meta(&meta_path).ok().flatten();
        let expected_sha = meta.as_ref().map(|m| m.last_written_sha256.as_str());

        let agent_wrote_it = matches!(expected_sha, Some(s) if s == on_disk_sha);
        if !agent_wrote_it && !force {
            return Err(ManualEditDetected {
                path: path.clone(),
                on_disk_sha256: on_disk_sha,
                expected_sha256: expected_sha.unwrap_or("<no meta.toml>").to_string(),
            }
            .into());
        }
    }

    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("creating stack dir {}: {e}", dir.display()))?;

    // Atomic write: tmp + fsync + rename.
    let tmp_path = dir.join("compose.yaml.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| anyhow::anyhow!("open {} for write: {e}", tmp_path.display()))?;
        f.write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| anyhow::anyhow!("fsync {}: {e}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, &path)
        .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", tmp_path.display(), path.display()))?;

    // Preserve `last_applied_sha256` across a fresh write so the watcher
    // path can still recognise its own self-trigger after a refresh.
    let prior_applied = read_meta(&meta_path)
        .ok()
        .flatten()
        .and_then(|m| m.last_applied_sha256);
    let meta = ComposeMeta {
        source: "imported".to_string(),
        imported_at: imported_at_rfc3339.to_string(),
        last_written_sha256: new_sha,
        host_id: host_id.to_string(),
        last_applied_sha256: prior_applied,
    };
    write_meta(&meta_path, &meta)?;

    Ok(WriteOutcome::Written)
}

/// Update the `last_applied_sha256` field in `meta.toml` after a
/// successful reconcile. Creates the meta record if missing (the
/// watcher is the first writer for v0.3d-managed stacks that pre-date
/// v0.3c import).
pub fn record_last_applied(dir: &Path, sha: &str) -> Result<(), anyhow::Error> {
    let meta_path = dir.join("meta.toml");
    let mut meta = read_meta(&meta_path)
        .ok()
        .flatten()
        .unwrap_or_else(|| ComposeMeta {
            source: "managed".to_string(),
            imported_at: chrono::Utc::now().to_rfc3339(),
            last_written_sha256: sha.to_string(),
            host_id: String::new(),
            last_applied_sha256: None,
        });
    meta.last_applied_sha256 = Some(sha.to_string());
    write_meta(&meta_path, &meta)
}

/// Returns `Ok(true)` when `sha` matches the recorded
/// `last_applied_sha256` in `dir/meta.toml`. Used by the v0.3d watcher
/// to skip reconcile loops triggered by the agent's own writes.
pub fn matches_last_applied(dir: &Path, sha: &str) -> Result<bool, anyhow::Error> {
    let meta_path = dir.join("meta.toml");
    Ok(matches!(
        read_meta(&meta_path)?.and_then(|m| m.last_applied_sha256),
        Some(s) if s == sha
    ))
}

/// Read the current on-disk YAML for a stack. Returns the body plus its
/// sha256, or `Ok(None)` when the file doesn't exist yet.
pub fn read_compose(dir: &Path) -> Result<Option<(String, String)>, anyhow::Error> {
    let path = dir.join("compose.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let sha = sha256_hex(&body);
    Ok(Some((body, sha)))
}

/// Outcome of a v0.3d controller-driven `WriteCompose`. Used by the
/// agent's sync handler to build the `WriteComposeAck` proto reply.
#[derive(Debug, Clone)]
pub enum ApplyWriteOutcome {
    /// Wrote the YAML; carries the sha256 of the new file.
    Ok { written_sha256: String },
    /// On-disk hash didn't match `expected_sha256` and `force` was
    /// false. The dashboard renders a diff using the included current
    /// state.
    Conflict {
        current_sha256: String,
        current_yaml: String,
    },
    /// I/O error, parse failure, etc.
    Error(String),
}

/// Apply a controller-driven `WriteCompose`. Verifies the optimistic
/// concurrency check (`expected_sha256` matches what's on disk) unless
/// `force = true`, then atomically writes the new YAML + meta.toml.
///
/// When `manifest_toml` is non-empty, also persists it at
/// `dir/stack.toml` (atomic write). The agent does NOT parse it; it's
/// kept for operator inspection + `isd manifest cat` (future). Hook
/// execution + secret threading live elsewhere; this function only
/// owns the disk surface.
pub fn apply_controller_write(
    dir: &Path,
    new_yaml: &str,
    expected_sha256: &str,
    host_id: &str,
    force: bool,
    manifest_toml: &str,
) -> ApplyWriteOutcome {
    // Conflict check: only if a file exists. First-time writes from the
    // dashboard pass `expected_sha256 = ""`, which we accept.
    match read_compose(dir) {
        Ok(Some((current_yaml, current_sha))) => {
            if !force && expected_sha256 != current_sha {
                return ApplyWriteOutcome::Conflict {
                    current_sha256: current_sha,
                    current_yaml,
                };
            }
        }
        Ok(None) => {} // first-time write, nothing to compare against
        Err(e) => return ApplyWriteOutcome::Error(format!("{e:#}")),
    }

    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = write_compose(dir, new_yaml, host_id, &now, true) {
        return ApplyWriteOutcome::Error(format!("{e:#}"));
    }

    // Manifest persistence. Non-fatal: a manifest write
    // failure logs but does NOT roll back the compose write (the deploy
    // already happened; the operator can re-deploy to retry the manifest).
    if !manifest_toml.is_empty() {
        if let Err(e) = write_manifest_atomic(dir, manifest_toml) {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "failed to persist stack.toml; compose write succeeded",
            );
        }
    }

    ApplyWriteOutcome::Ok {
        written_sha256: sha256_hex(new_yaml),
    }
}

/// Atomic write of `dir/stack.toml`.
fn write_manifest_atomic(dir: &Path, manifest_toml: &str) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join("stack.toml");
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("open {}: {e}", tmp.display()))?;
        f.write_all(manifest_toml.as_bytes())
            .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| anyhow::anyhow!("fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

fn write_meta(path: &Path, meta: &ComposeMeta) -> Result<(), anyhow::Error> {
    let body =
        toml::to_string_pretty(meta).map_err(|e| anyhow::anyhow!("serialize meta.toml: {e}"))?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("open {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| anyhow::anyhow!("fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

fn read_meta(path: &Path) -> Result<Option<ComposeMeta>, anyhow::Error> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let meta: ComposeMeta =
        toml::from_str(&body).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_write_creates_file_and_meta() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        let outcome = write_compose(
            &stack_dir,
            "version: '3'\n",
            "01ARZHOST",
            "2026-05-07T00:00:00Z",
            false,
        )
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        let on_disk = std::fs::read_to_string(stack_dir.join("compose.yaml")).unwrap();
        assert_eq!(on_disk, "version: '3'\n");

        let meta_str = std::fs::read_to_string(stack_dir.join("meta.toml")).unwrap();
        let meta: ComposeMeta = toml::from_str(&meta_str).unwrap();
        assert_eq!(meta.source, "imported");
        assert_eq!(meta.host_id, "01ARZHOST");
        assert_eq!(meta.last_written_sha256, sha256_hex("version: '3'\n"));
    }

    #[test]
    fn second_write_with_same_content_is_unchanged() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        write_compose(&stack_dir, "x: 1\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        let outcome =
            write_compose(&stack_dir, "x: 1\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
    }

    #[test]
    fn agent_overwrite_after_its_own_write_succeeds() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        write_compose(&stack_dir, "x: 1\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        let outcome =
            write_compose(&stack_dir, "x: 2\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        let on_disk = std::fs::read_to_string(stack_dir.join("compose.yaml")).unwrap();
        assert_eq!(on_disk, "x: 2\n");
    }

    #[test]
    fn refuses_to_overwrite_manual_edit_without_force() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        write_compose(&stack_dir, "x: 1\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        // Simulate a user editing the file out from under the agent.
        std::fs::write(stack_dir.join("compose.yaml"), "x: edited-by-hand\n").unwrap();

        let err =
            write_compose(&stack_dir, "x: 2\n", "h", "2026-05-07T00:00:00Z", false).unwrap_err();
        let downcast = err
            .downcast_ref::<ManualEditDetected>()
            .expect("expected ManualEditDetected");
        assert!(downcast.on_disk_sha256 != downcast.expected_sha256);
    }

    #[test]
    fn force_clobbers_manual_edit() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        write_compose(&stack_dir, "x: 1\n", "h", "2026-05-07T00:00:00Z", false).unwrap();
        std::fs::write(stack_dir.join("compose.yaml"), "x: edited-by-hand\n").unwrap();

        let outcome =
            write_compose(&stack_dir, "x: 2\n", "h", "2026-05-07T00:00:00Z", true).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        let on_disk = std::fs::read_to_string(stack_dir.join("compose.yaml")).unwrap();
        assert_eq!(on_disk, "x: 2\n");
    }

    #[test]
    fn refuses_when_compose_present_but_no_meta_toml() {
        let dir = tempdir().unwrap();
        let stack_dir = dir.path().join("hello");
        std::fs::create_dir_all(&stack_dir).unwrap();
        std::fs::write(stack_dir.join("compose.yaml"), "x: from-elsewhere\n").unwrap();

        let err =
            write_compose(&stack_dir, "x: 2\n", "h", "2026-05-07T00:00:00Z", false).unwrap_err();
        assert!(err.downcast_ref::<ManualEditDetected>().is_some());
    }
}
