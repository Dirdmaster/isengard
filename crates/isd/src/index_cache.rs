//! The `isd` index cache: `~/.cache/isengard/last-ps.json`.
//!
//! Any list command that renders a `#` column writes its row set here.
//! Lifecycle commands read it back to resolve a bare integer
//! argument (`isd stop 2`) to a container. This module owns the schema,
//! the read/write, and the staleness check; it does not do resolution.
//!
//! Currently only `write` is exercised; `read`, `age_secs`, `is_stale`,
//! and `STALE_AFTER_SECS` are the API surface the resolver
//! will call. They're covered by unit tests in this module but not yet
//! by a consumer in `main.rs`, so `dead_code` is allowed module-wide
//! until the resolver wires them in.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-15-isd-table-renderer-design.md`.

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

/// Path to the cache file. `~/.cache/isengard/last-ps.json` by default;
/// overridable with `ISD_INDEX_CACHE` for tests.
pub fn cache_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ISD_INDEX_CACHE") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::cache_dir().context("could not resolve the user cache directory")?;
    Ok(base.join("isengard").join("last-ps.json"))
}

/// Write the cache, creating the parent directory if needed.
pub fn write(cache: &IndexCache) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating index cache dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cache).context("serializing index cache")?;
    std::fs::write(&path, json)
        .with_context(|| format!("writing index cache {}", path.display()))?;
    Ok(())
}

/// Read the cache. `Ok(None)` when the file does not exist (absence is
/// not an error here; the caller decides what to do). `Err` on a
/// present-but-unreadable or corrupt file.
pub fn read() -> Result<Option<IndexCache>> {
    let path = cache_path()?;
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

    /// Point the cache at a unique temp path for the duration of a test.
    /// SAFETY: matches the `ISD_CREDENTIALS_FILE` test pattern in
    /// `ps.rs` / `manifest_cmd.rs`; each test uses a distinct path.
    /// Callers must hold `test_env_lock()` for the duration.
    fn with_temp_cache(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", &path);
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
        let _dir = with_temp_cache("rt.json");
        let cache = sample();
        write(&cache).unwrap();
        let back = read().unwrap().expect("cache present after write");
        assert_eq!(back.command, "ps");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[1].container_id, "7f8e9d0c1b2acafef00d");
        assert_eq!(back.rows[0].name, "web-proxy");
    }

    #[test]
    fn read_missing_file_is_ok_none() {
        let _lock = test_env_lock();
        let _dir = with_temp_cache("does-not-exist.json");
        assert!(read().unwrap().is_none());
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
