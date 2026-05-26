//! The `isd` index cache. Each list command that renders a `#` column
//! writes its row set to its own file under `~/.cache/isengard/`:
//!
//! - `last-ps.json`         (`isd ps`)
//! - `last-ssh-hosts.json`  (`isd ssh hosts`)
//! - `last-stack-ps.json`   (`isd stack ps`)
//! - `last-stack-ls.json`   (`isd stack ls`)
//!
//! Per-command files keep an `isd ssh 1` dial reading from the freshest
//! `isd ssh hosts` render even after an `isd ps` ran in between. Before
//! this split, every list command overwrote the same `last-ps.json` and
//! a `ssh <N>` could pull a 64-char container hash instead of a dial
//! target.
//!
//! Lifecycle commands read the matching cache back to resolve a bare
//! integer argument (`isd stop 2`) to a container. This module owns the
//! schema, the read/write, and the staleness check; it does not do
//! resolution.
//!
//! Caches row indices so follow-up commands can resolve operator shorthand.

#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Resolution older than this many seconds prints a staleness warning
/// (it never blocks). Spec decision 5.
pub const STALE_AFTER_SECS: i64 = 60;

/// The on-disk cache document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCache {
    /// When the producing list command ran. Drives staleness.
    pub captured_at: DateTime<Utc>,
    /// Which list command produced the rows (`ps`, `stack ps`, ...).
    /// Diagnostic only; resolution ignores it.
    pub command: String,
    /// The rows, in render order. `rows[i].index == i`.
    pub rows: Vec<IndexRow>,
}

/// One cached row: enough to target the container later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRow {
    /// Render-order index, the `#` column value.
    pub index: usize,
    /// Context (host) the container lives on. Makes the index
    /// host-aware for multi-host fanout.
    pub context: String,
    /// Full resolved container ID. Stored, never re-derived, so a
    /// stale index can never silently re-point to a different
    /// container.
    pub container_id: String,
    /// Container name, for the destructive-confirm display in 0.22.
    pub name: String,
}

impl IndexCache {
    /// Seconds since `captured_at`, clamped at 0.
    pub fn age_secs(&self) -> i64 {
        (Utc::now() - self.captured_at).num_seconds().max(0)
    }

    /// True when the cache is older than [`STALE_AFTER_SECS`].
    pub fn is_stale(&self) -> bool {
        self.age_secs() > STALE_AFTER_SECS
    }
}

/// Directory that holds every per-command index cache file.
/// `~/.cache/isengard/` by default; overridable with `ISD_INDEX_CACHE`
/// for tests (the env var now names a *directory*, not a file).
pub fn cache_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ISD_INDEX_CACHE") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::cache_dir().context("could not resolve the user cache directory")?;
    Ok(base.join("isengard"))
}

/// Filename slug for a given `command` string. Spaces become dashes so
/// `ssh hosts` -> `ssh-hosts`. Callers should use the same string they
/// stored in [`IndexCache::command`].
fn slug_for(command: &str) -> String {
    command.replace(' ', "-")
}

/// Path to the cache file for a given command. Examples:
///
/// - `cache_path_for("ps")`        -> `<dir>/last-ps.json`
/// - `cache_path_for("ssh hosts")` -> `<dir>/last-ssh-hosts.json`
/// - `cache_path_for("stack ps")`  -> `<dir>/last-stack-ps.json`
pub fn cache_path_for(command: &str) -> Result<PathBuf> {
    let slug = slug_for(command);
    Ok(cache_dir()?.join(format!("last-{slug}.json")))
}

/// Write the cache to the file matching `cache.command`. Creates the
/// parent directory if needed.
pub fn write(cache: &IndexCache) -> Result<()> {
    let path = cache_path_for(&cache.command)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating index cache dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cache).context("serializing index cache")?;
    std::fs::write(&path, json)
        .with_context(|| format!("writing index cache {}", path.display()))?;
    Ok(())
}

/// Build the operator-facing out-of-range message used by every index
/// resolver. Keeps the 0-indexed convention obvious:
///
/// - 1 row:  `index N out of range. Last 'isd X' had 1 row (index 0).`
/// - N rows: `index N out of range. Last 'isd X' had K rows (indices 0..K-1).`
///
/// `command_hint` is the command-shaped string the operator can re-run,
/// e.g. `"isd ps"` or `"isd ssh hosts"`. `row_count` is the size of the
/// last render (NOT the bad index).
pub fn out_of_range_message(idx: usize, command_hint: &str, row_count: usize) -> String {
    if row_count == 0 {
        return format!(
            "index {idx} out of range. Last `{command_hint}` had 0 rows; run it first."
        );
    }
    if row_count == 1 {
        return format!(
            "index {idx} out of range. Last `{command_hint}` had 1 row (index 0). Use `0` to pick the first row."
        );
    }
    let last = row_count - 1;
    format!(
        "index {idx} out of range. Last `{command_hint}` had {row_count} rows (indices 0..{last})."
    )
}

/// Read the cache for a specific command. `Ok(None)` when the file does
/// not exist (absence is not an error here; the caller decides what to
/// do). `Err` on a present-but-unreadable or corrupt file.
pub fn read_for(command: &str) -> Result<Option<IndexCache>> {
    let path = cache_path_for(command)?;
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            let cache = serde_json::from_str(&body)
                .with_context(|| format!("parsing index cache {}", path.display()))?;
            Ok(Some(cache))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading index cache {}", path.display())),
    }
}

/// Process-wide guard for tests that touch `ISD_INDEX_CACHE`. Cargo
/// runs tests in parallel; the env var is process-global, so two tests
/// fighting over it will see each other's writes. Every test in the
/// crate that mutates `ISD_INDEX_CACHE` acquires this lock first.
/// Pattern mirrors `crates/isengard-plugins/dashboard/tests/approvals_endpoints.rs`.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the cache *directory* at a unique temp dir for the
    /// duration of a test. `ISD_INDEX_CACHE` names a directory now;
    /// per-command files live inside it.
    /// SAFETY: matches the `ISD_CREDENTIALS_FILE` test pattern in
    /// `ps.rs` / `manifest_cmd.rs`; each test uses a distinct dir.
    /// Callers must hold `test_env_lock()` for the duration.
    fn with_temp_cache() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", dir.path());
        }
        dir
    }

    fn sample() -> IndexCache {
        IndexCache {
            captured_at: Utc::now(),
            command: "ps".into(),
            rows: vec![
                IndexRow {
                    index: 0,
                    context: "lausanne".into(),
                    container_id: "a1b2c3d4e5f6deadbeef".into(),
                    name: "web-proxy".into(),
                },
                IndexRow {
                    index: 1,
                    context: "lausanne".into(),
                    container_id: "7f8e9d0c1b2acafef00d".into(),
                    name: "app-db".into(),
                },
            ],
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let _lock = test_env_lock();
        let _dir = with_temp_cache();
        let cache = sample();
        write(&cache).unwrap();
        let back = read_for("ps").unwrap().expect("cache present after write");
        assert_eq!(back.command, "ps");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[1].container_id, "7f8e9d0c1b2acafef00d");
        assert_eq!(back.rows[0].name, "web-proxy");
    }

    #[test]
    fn read_for_missing_file_is_ok_none() {
        let _lock = test_env_lock();
        let _dir = with_temp_cache();
        assert!(read_for("ps").unwrap().is_none());
    }

    /// The core bug fix: writing one command's cache must NOT leak into
    /// another command's cache. `isd ps` writes its rows; a later
    /// `isd ssh <N>` must NOT see those rows under the `ssh hosts` key.
    #[test]
    fn read_for_returns_none_when_only_a_different_command_was_written() {
        let _lock = test_env_lock();
        let _dir = with_temp_cache();
        let mut ps_cache = sample();
        ps_cache.command = "ps".into();
        write(&ps_cache).unwrap();

        assert!(read_for("ps").unwrap().is_some());
        assert!(
            read_for("ssh hosts").unwrap().is_none(),
            "ssh hosts cache must NOT pick up ps rows"
        );
    }

    #[test]
    fn cache_path_for_uses_command_slug() {
        let _lock = test_env_lock();
        let _dir = with_temp_cache();
        let ps = cache_path_for("ps").unwrap();
        let ssh = cache_path_for("ssh hosts").unwrap();
        let stack_ps = cache_path_for("stack ps").unwrap();
        let stack_ls = cache_path_for("stack ls").unwrap();
        assert!(ps.ends_with("last-ps.json"), "got {}", ps.display());
        assert!(
            ssh.ends_with("last-ssh-hosts.json"),
            "got {}",
            ssh.display()
        );
        assert!(
            stack_ps.ends_with("last-stack-ps.json"),
            "got {}",
            stack_ps.display()
        );
        assert!(
            stack_ls.ends_with("last-stack-ls.json"),
            "got {}",
            stack_ls.display()
        );
    }

    #[test]
    fn fresh_cache_is_not_stale_old_cache_is() {
        let fresh = sample();
        assert!(!fresh.is_stale());

        let mut old = sample();
        old.captured_at = Utc::now() - chrono::Duration::seconds(STALE_AFTER_SECS + 30);
        assert!(old.is_stale());
        assert!(old.age_secs() >= STALE_AFTER_SECS);
    }
}
