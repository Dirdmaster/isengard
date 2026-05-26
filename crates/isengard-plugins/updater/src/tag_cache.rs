//! Per-image tag cache + semver helpers for the `Minor` update strategy
//!
//! See spec §"Per-image tag cache" + §"Semver compare" of
//! Used by the minor-version update strategy.
//!
//! Two surfaces:
//!
//! - [`TagCache`]: in-memory, TTL-bounded memoization of `list_tags` results
//!   keyed by `<registry>/<repository>`. Concurrent calls for the same key
//!   share the in-flight fetch.
//! - [`pick_highest_minor`]: pure function picking the highest patch+minor
//!   tag with a major matching `current`, ignoring prereleases and tags
//!   that don't parse as semver.
//!
//! TTL is passive: stale entries are detected on read and refreshed in
//! place. There is no background sweeper; on a homelab the live image set
//! is bounded.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use semver::{Prerelease, Version};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use crate::image_ref::ImageRef;

/// Default TTL for cached tag lists. Spec calls for 1 hour.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// One cached `list_tags` result plus when it was fetched.
#[derive(Debug, Clone)]
struct CachedTags {
    /// Tag list shared with every reader for this key.
    tags: Arc<Vec<String>>,
    /// Wall-clock instant the fetch landed.
    fetched_at: Instant,
}

/// Per-image cache of registry tag lists. Cheap to clone via `Arc`.
pub struct TagCache {
    /// Outer: keyed by `<registry>/<repository>`. Each value is an async
    /// mutex wrapping the optional cached entry. The outer lock is a
    /// std::Mutex held only briefly to look up / insert; the inner
    /// async mutex is held across the fetch await so concurrent callers
    /// for the same key share the result.
    entries: StdMutex<HashMap<String, Arc<AsyncMutex<Option<CachedTags>>>>>,
    /// Cache entry lifetime.
    ttl: std::time::Duration,
}

impl std::fmt::Debug for TagCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagCache")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl TagCache {
    /// Build a cache with the given TTL.
    pub fn new(ttl: std::time::Duration) -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Build a cache with the default TTL (1h).
    pub fn with_default_ttl() -> Self {
        Self::new(std::time::Duration::from_secs(DEFAULT_TTL_SECS))
    }

    /// Builds the per-image cache key.
    fn key(image: &ImageRef) -> String {
        format!("{}/{}", image.registry, image.repository)
    }

    /// Return the cached tag list for `image`, or fetch + cache via
    /// `fetch` if absent or expired. Concurrent calls for the same key
    /// share the same fetch future.
    ///
    /// `fetch` is invoked on cache miss (and on stale hit). The
    /// returned `Vec<String>` is wrapped in an `Arc` so all readers
    /// share the same allocation.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        image: &ImageRef,
        fetch: F,
    ) -> anyhow::Result<Arc<Vec<String>>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<Vec<String>>>,
    {
        let key = Self::key(image);
        // Get-or-insert the per-key async mutex. Outer lock held only
        // long enough to clone the Arc.
        let slot = {
            let mut map = self.entries.lock().expect("tag cache mutex poisoned");
            map.entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
                .clone()
        };

        // Hold the inner mutex across the (possibly slow) fetch so two
        // racing callers don't both hit the registry.
        let mut guard = slot.lock().await;
        if let Some(entry) = guard.as_ref() {
            if entry.fetched_at.elapsed() < self.ttl {
                return Ok(Arc::clone(&entry.tags));
            }
        }

        let fresh = fetch().await?;
        let arc = Arc::new(fresh);
        *guard = Some(CachedTags {
            tags: Arc::clone(&arc),
            fetched_at: Instant::now(),
        });
        Ok(arc)
    }
}

/// Strip a single leading `v` (or `V`) so `v1.2.3` parses identically to
/// `1.2.3`. Returns the original slice if no prefix is present.
fn strip_v_prefix(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s)
}

/// Parse a tag string into a semver [`Version`]. Strips one leading
/// `v`/`V` first. Returns `None` if the result doesn't parse.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(strip_v_prefix(tag)).ok()
}

/// Pick the highest tag from `tags` that:
///
/// - parses as semver (after stripping a leading `v`),
/// - has the same major as `current`,
/// - is not a pre-release,
/// - is strictly greater than `current`.
///
/// Returns `None` if no tag qualifies. Pure; no I/O.
pub fn pick_highest_minor(tags: &[String], current: &Version) -> Option<Version> {
    let mut best: Option<Version> = None;
    for raw in tags {
        let Some(v) = parse_tag(raw) else { continue };
        if v.major != current.major {
            continue;
        }
        if v.pre != Prerelease::EMPTY {
            continue;
        }
        if v <= *current {
            continue;
        }
        match best.as_ref() {
            None => best = Some(v),
            Some(b) if v > *b => best = Some(v),
            _ => {}
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn img(repo: &str) -> ImageRef {
        ImageRef::parse(repo).expect("parse")
    }

    // -------- pick_highest_minor (5+ tests) --------

    #[test]
    fn picks_patch_bump_within_same_minor() {
        let tags: Vec<String> = ["1.2.3", "1.2.4"].iter().map(|s| s.to_string()).collect();
        let current = Version::parse("1.2.3").unwrap();
        let picked = pick_highest_minor(&tags, &current).unwrap();
        assert_eq!(picked, Version::parse("1.2.4").unwrap());
    }

    #[test]
    fn picks_minor_bump_over_patch() {
        let tags: Vec<String> = ["1.2.3", "1.2.4", "1.3.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let current = Version::parse("1.2.3").unwrap();
        let picked = pick_highest_minor(&tags, &current).unwrap();
        assert_eq!(picked, Version::parse("1.3.0").unwrap());
    }

    #[test]
    fn refuses_major_bump() {
        let tags: Vec<String> = ["1.2.3", "2.0.0"].iter().map(|s| s.to_string()).collect();
        let current = Version::parse("1.2.3").unwrap();
        // Only 1.2.3 in the same major, and it's not strictly greater.
        assert!(pick_highest_minor(&tags, &current).is_none());
    }

    #[test]
    fn ignores_prerelease_tags() {
        let tags: Vec<String> = ["1.2.3", "1.3.0-rc.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let current = Version::parse("1.2.3").unwrap();
        // 1.3.0-rc.1 must be skipped; nothing else qualifies.
        assert!(pick_highest_minor(&tags, &current).is_none());
    }

    #[test]
    fn drops_malformed_tags() {
        let tags: Vec<String> = ["latest", "nightly", "1.2.4", "not.a.version"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let current = Version::parse("1.2.3").unwrap();
        let picked = pick_highest_minor(&tags, &current).unwrap();
        assert_eq!(picked, Version::parse("1.2.4").unwrap());
    }

    #[test]
    fn strips_leading_v_prefix() {
        let tags: Vec<String> = ["v1.2.3", "v1.2.4"].iter().map(|s| s.to_string()).collect();
        let current = parse_tag("v1.2.3").unwrap();
        let picked = pick_highest_minor(&tags, &current).unwrap();
        assert_eq!(picked, Version::parse("1.2.4").unwrap());
    }

    #[test]
    fn returns_none_when_current_is_already_highest() {
        let tags: Vec<String> = ["1.2.3", "1.2.4"].iter().map(|s| s.to_string()).collect();
        let current = Version::parse("1.2.4").unwrap();
        assert!(pick_highest_minor(&tags, &current).is_none());
    }

    // -------- TagCache (4+ tests) --------

    #[tokio::test(start_paused = true)]
    async fn cache_miss_calls_fetcher_once() {
        let cache = TagCache::new(Duration::from_secs(60));
        let count = Arc::new(AtomicUsize::new(0));

        let result = cache
            .get_or_fetch(&img("nginx:1.2.3"), || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(vec!["1.2.3".to_string(), "1.2.4".to_string()])
                }
            })
            .await
            .unwrap();
        assert_eq!(
            result.as_slice(),
            &["1.2.3".to_string(), "1.2.4".to_string()]
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_hit_does_not_refetch() {
        let cache = TagCache::new(Duration::from_secs(60));
        let count = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let count = count.clone();
            let _ = cache
                .get_or_fetch(&img("nginx:1.2.3"), || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(vec!["1.2.3".to_string()])
                    }
                })
                .await
                .unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entry_triggers_refetch() {
        let cache = TagCache::new(Duration::from_secs(60));
        let count = Arc::new(AtomicUsize::new(0));

        let fetcher = || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["1.2.3".to_string()])
            }
        };

        let _ = cache
            .get_or_fetch(&img("nginx:1.2.3"), fetcher)
            .await
            .unwrap();
        // Advance virtual time past TTL.
        tokio::time::advance(Duration::from_secs(120)).await;
        let fetcher2 = || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["1.2.3".to_string(), "1.2.4".to_string()])
            }
        };
        let _ = cache
            .get_or_fetch(&img("nginx:1.2.3"), fetcher2)
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_calls_share_fetch_result() {
        // h1 enters the cache, holds the inner per-key mutex, and waits
        // on a oneshot before completing its fetch. h2 enters after h1
        // is parked: it must block on the same per-key mutex until h1
        // releases. When h1 releases, h2 sees the populated entry and
        // skips its own fetcher entirely.
        //
        // The structural invariant we assert: only h1's fetcher runs
        // exactly once; h2's fetcher must not.
        let cache = Arc::new(TagCache::new(Duration::from_secs(60)));
        let count_h1 = Arc::new(AtomicUsize::new(0));
        let count_h2 = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let c1 = cache.clone();
        let cnt1 = count_h1.clone();
        let h1 = tokio::spawn(async move {
            c1.get_or_fetch(&img("nginx:1.2.3"), move || async move {
                cnt1.fetch_add(1, Ordering::SeqCst);
                // Block until the test releases us.
                let _ = release_rx.await;
                Ok(vec!["1.2.3".to_string()])
            })
            .await
        });

        // Yield until h1 has entered the cache and is parked on the
        // oneshot. Polling `count_h1` is the simplest synchronization:
        // h1 increments it BEFORE awaiting the oneshot. Once we see the
        // increment, h1 has the inner per-key mutex.
        for _ in 0..256 {
            if count_h1.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(count_h1.load(Ordering::SeqCst), 1, "h1 fetcher must enter");

        let c2 = cache.clone();
        let cnt2 = count_h2.clone();
        let h2 = tokio::spawn(async move {
            c2.get_or_fetch(&img("nginx:1.2.3"), move || async move {
                cnt2.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["should-not-run".to_string()])
            })
            .await
        });

        // Give h2 a moment to attempt the lock and block on it.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Release h1; both tasks now finish.
        release_tx.send(()).expect("send release");

        let r1 = h1.await.unwrap().unwrap();
        let r2 = h2.await.unwrap().unwrap();
        assert_eq!(r1.as_slice(), &["1.2.3".to_string()]);
        assert_eq!(r2.as_slice(), &["1.2.3".to_string()]);
        assert_eq!(count_h1.load(Ordering::SeqCst), 1);
        assert_eq!(
            count_h2.load(Ordering::SeqCst),
            0,
            "h2's fetcher must not run when h1's fetch is in-flight"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn separate_keys_use_separate_entries() {
        let cache = TagCache::new(Duration::from_secs(60));
        let count = Arc::new(AtomicUsize::new(0));

        for repo in ["nginx:1.2.3", "redis:7.0"] {
            let count = count.clone();
            let _ = cache
                .get_or_fetch(&img(repo), || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(vec!["1.0.0".to_string()])
                    }
                })
                .await
                .unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
