//! Resolve positional command-line args to docker container targets.
//! Args that look like decimal index selectors (per `selector`) are
//! resolved against the index cache produced by `isd ps`; everything
//! else passes through unchanged as a docker ID or name.
//!
//! Emits the staleness warning when the index cache is older than the
//! threshold defined in `index_cache::STALE_AFTER_SECS`. Resolution
//! never blocks on staleness.
//!
//! Default-and-document: this module is consumed by the lifecycle
//! commands (Tasks 4-7 of the plan); until those land in
//! the same PR, `dead_code` is allowed module-wide so clippy stays
//! green between commits. Mirrors `index_cache.rs` and `selector.rs`.

#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};

use crate::index_cache::{self, IndexCache, IndexRow};
use crate::selector;

/// One resolved target. `via_index` flips the destructive-confirm
/// prompt: index-resolved targets ask before acting; literal IDs
/// don't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Full docker container ID. For literal-name args this is the
    /// raw arg (the daemon does its own lookup).
    pub container_id: String,
    /// Container name for display in confirm prompts. Equals
    /// `container_id` when the arg was a literal pass-through.
    pub name: String,
    /// Context (host) the container lives on. Empty when the arg was
    /// a literal pass-through (we have no host info without the index
    /// cache).
    pub context: String,
    /// True when this target came from an index selector. Drives the
    /// confirm prompt and the staleness check.
    pub via_index: bool,
}

/// Resolve every positional arg into one or more targets.
///
/// A literal ID/name produces one target with `name = arg.to_string()`
/// (we do not reverse-lookup the container's name from the daemon to
/// keep this pure and offline). A selector produces one target per
/// index. Selector args require the index cache; literal-only args
/// don't.
///
/// # Errors
///
/// Returns `Err` when any selector arg references an out-of-range
/// index, when a selector arg is supplied but no cache exists, or
/// when the cache exists but is unreadable. Stale-but-valid caches
/// log a warning to stderr and proceed.
pub fn resolve(args: &[String]) -> Result<Vec<ResolvedTarget>> {
    let cache: Option<IndexCache> = index_cache::read().context("reading index cache")?;

    let mut want_indices = false;
    for arg in args {
        if selector::looks_like_selector(arg) {
            want_indices = true;
            break;
        }
    }

    if want_indices && cache.is_none() {
        return Err(anyhow!(
            "no index cache found at {}; run `isd ps` first",
            index_cache::cache_path()?.display()
        ));
    }

    if let Some(c) = cache.as_ref()
        && want_indices
        && c.is_stale()
    {
        eprintln!(
            "isd: index cache is {} seconds old; run `isd ps` to refresh",
            c.age_secs()
        );
    }

    let mut out = Vec::new();
    for arg in args {
        if selector::looks_like_selector(arg) {
            let indices = selector::parse_token(arg)?;
            let cache = cache.as_ref().expect("checked above");
            for idx in indices {
                let row = lookup(cache, idx)?;
                out.push(ResolvedTarget {
                    container_id: row.container_id.clone(),
                    name: row.name.clone(),
                    context: row.context.clone(),
                    via_index: true,
                });
            }
        } else {
            out.push(ResolvedTarget {
                container_id: arg.clone(),
                name: arg.clone(),
                context: String::new(),
                via_index: false,
            });
        }
    }
    Ok(out)
}

/// Find the row with `index == idx` in the cache, or return an error
/// naming the valid range. Separated out of [`resolve`] so the
/// out-of-range message stays in one place.
fn lookup(cache: &IndexCache, idx: usize) -> Result<&IndexRow> {
    cache.rows.iter().find(|r| r.index == idx).ok_or_else(|| {
        let max = cache.rows.iter().map(|r| r.index).max().unwrap_or(0);
        anyhow!("index {idx} out of range; last `isd ps` had 0-{max}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Process-wide env-lock shared with `index_cache::tests` so both
    /// modules' tests serialize their access to `ISD_INDEX_CACHE`. See
    /// `index_cache::test_env_lock` for the rationale.
    use crate::index_cache::test_env_lock as env_lock;

    fn with_cache(rows: Vec<IndexRow>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-ps.json");
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", &path);
        }
        let cache = IndexCache {
            captured_at: Utc::now(),
            command: "ps".into(),
            rows,
        };
        index_cache::write(&cache).unwrap();
        dir
    }

    fn sample_rows() -> Vec<IndexRow> {
        vec![
            IndexRow {
                index: 0,
                context: "lausanne".into(),
                container_id: "a1b2c3d4e5f6".into(),
                name: "web-proxy".into(),
            },
            IndexRow {
                index: 1,
                context: "lausanne".into(),
                container_id: "7f8e9d0c1b2a".into(),
                name: "app-db".into(),
            },
            IndexRow {
                index: 2,
                context: "lausanne".into(),
                container_id: "3c4d5e6f7a8b".into(),
                name: "isd-agent".into(),
            },
        ]
    }

    #[test]
    fn literal_ids_pass_through() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let r = resolve(&["a1b2c3d4e5f6".into()]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].container_id, "a1b2c3d4e5f6");
        assert!(!r[0].via_index);
    }

    #[test]
    fn single_index_resolves() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let r = resolve(&["1".into()]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].container_id, "7f8e9d0c1b2a");
        assert_eq!(r[0].name, "app-db");
        assert!(r[0].via_index);
    }

    #[test]
    fn range_and_list_resolve() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let r = resolve(&["0-2".into()]).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].name, "web-proxy");
        assert_eq!(r[2].name, "isd-agent");
    }

    #[test]
    fn mixed_indices_and_literals_preserve_order() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let r = resolve(&["0".into(), "a1b2c3d4e5f6".into(), "2".into()]).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r[0].via_index);
        assert!(!r[1].via_index);
        assert!(r[2].via_index);
    }

    #[test]
    fn out_of_range_index_errors_with_max() {
        let _lock = env_lock();
        let _dir = with_cache(sample_rows());
        let err = resolve(&["9".into()]).unwrap_err().to_string();
        assert!(err.contains("out of range"), "got: {err}");
        assert!(err.contains("0-2"), "should name the valid range: {err}");
    }

    #[test]
    fn missing_cache_errors_with_pointer() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", &path);
        }
        let err = resolve(&["0".into()]).unwrap_err().to_string();
        assert!(err.contains("no index cache"), "got: {err}");
    }

    #[test]
    fn literal_only_args_do_not_require_cache() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        unsafe {
            std::env::set_var("ISD_INDEX_CACHE", &path);
        }
        // No selector token present: works even without a cache.
        let r = resolve(&["my-container".into()]).unwrap();
        assert_eq!(r.len(), 1);
    }
}
