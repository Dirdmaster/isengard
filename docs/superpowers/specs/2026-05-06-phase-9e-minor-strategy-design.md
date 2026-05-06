# Phase 9e: Minor Strategy (semver-aware tag bumping)

Source design: vault [[Update Policies & Approval Flow]] §"Implementation phasing" row 9i and §"Open questions" item 6.

The repo's previous numbering called approval-flow + telegram callbacks "9e/9f"; the vault design calls semver-aware bumping "9i". The same work is being implemented here under the issue label "9e (Minor strategy)" since the approval slice already shipped. Naming is a convention; the scope is what counts.

## What ships

`strategy=Minor` becomes a real, enforced policy value. Today the updater treats it identically to `TagOnly` (digest-only check). After this slice:

- The updater additionally lists tags from the registry's catalog, parses them as semver, and selects the highest patch+minor of the current image's major.
- If a higher tag exists, the updater treats THAT tag's digest as the proposed update. The `pending_approval` body and the recreate path both thread the bumped tag through.
- A per-image, in-memory cache (default 1h TTL) memoizes the tag list so the per-cycle path doesn't slam the registry.

## Out of scope

- Major-version bumps (`Any` strategy, ignored here).
- Manifest-list awareness (we don't need to inspect platform-specific tags; tag listing is sufficient for ranking).
- Persisting tag caches across process restarts (in-memory only; warm-up on first miss is fine).
- Wiring `Minor` semantics into the dashboard approval card UI (the existing card renders the digest pair; bumped-tag rendering stays a 9i+ ticket).

## Components

### 1. Registry tag listing

`crates/isengard-plugins/updater/src/registry.rs`:

```rust
impl RegistryClient {
    pub async fn list_tags(&self, image: &ImageRef) -> anyhow::Result<Vec<String>>;
}
```

Hits `GET /v2/<repo>/tags/list`. Same auth dance as `head_digest` (anonymous -> 401 -> bearer challenge -> retry). Honors pagination via the `Link: <...>; rel="next"` header (Docker registry v2 convention) up to a hard cap (5 pages, 5000 tags) so a misbehaving registry can't OOM the agent.

### 2. Per-image tag cache

`crates/isengard-plugins/updater/src/tag_cache.rs`:

```rust
pub struct TagCache {
    inner: Mutex<HashMap<String, CachedTags>>,
    ttl: Duration,
}

struct CachedTags {
    tags: Arc<Vec<String>>,
    fetched_at: Instant,
}

impl TagCache {
    pub fn new(ttl: Duration) -> Self;
    pub async fn get_or_fetch<F, Fut>(&self, image: &ImageRef, fetch: F) -> anyhow::Result<Arc<Vec<String>>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Vec<String>>>;
}
```

Cache key: `format!("{}/{}", image.registry, image.repository)`. Cache miss or expiry triggers `fetch()` and replaces the entry. Returning `Arc<Vec<String>>` lets concurrent readers share without cloning the inner Vec. Default TTL: 1h.

Eviction: passive; on access the entry is checked for staleness and replaced if expired. No background sweeper. This keeps memory bounded by the live image set, which on a homelab is small. We can revisit if a user reports leakage.

### 3. Semver compare

`crates/isengard-plugins/updater/src/tag_cache.rs` (sibling helper):

```rust
pub fn pick_highest_minor(tags: &[String], current: &Version) -> Option<Version>;
```

Steps: parse each tag with `semver::Version::parse`. Drop unparseable entries (silent; they may be `latest`, branch tags, etc.). Drop any entry whose major != `current.major`. Drop pre-releases (`Version::pre.is_empty() == false`) since users opting into `Minor` don't want `1.3.0-rc1` chosen. Sort descending. Return the highest if it is strictly greater than `current`; else `None`.

Tag prefix handling: many registries publish tags as `v1.2.3`. We strip a single leading `v` before parsing.

### 4. Updater integration

When `resolved.strategy == UpdateStrategy::Minor`:

1. After the digest check, regardless of whether digests match, list tags via the cache.
2. Parse the candidate's current tag (`image_ref.tag`). If unparseable, behave like `TagOnly` (no bump).
3. Pick the highest minor via `pick_highest_minor`. If `None`, behave like `TagOnly` (no bump).
4. If the picked tag != current and is strictly greater:
   - Build a new `ImageRef` with the bumped tag.
   - HEAD the new tag's digest via the registry (so we have a `proposed_digest` for the approval body / recreate).
   - Use that digest as the proposed update, even if the original tag's digest didn't change.

The existing `gate=Approval`/`Auto` decision then applies. The `PendingApprovalBody` carries the bumped image string (e.g. `ghcr.io/foo/bar:1.3.0`) and the bumped digest. The recreate path pulls the bumped tag.

### 5. Tests

| File | Test count | What it covers |
|---|---|---|
| `tag_cache.rs` (unit) | 4+ | hit, miss, expiry, concurrent access (two awaiting calls share the result) |
| `tag_cache.rs` (semver) | 5+ | 1.2.3 -> 1.2.4, 1.2.3 -> 1.3.0, no-major-bump 1.2.3 vs 2.0.0, prerelease ignored, malformed tags silently dropped |
| `registry.rs` (mocked HTTP) | 2+ | tags/list returns a list, paginated tags/list aggregates pages |
| `lib.rs` end-to-end | 1 | minor policy + new minor tag triggers update with the bumped tag (via injected fakes) |

## Decisions

- **semver crate version**: `1.0` (current stable).
- **Cache eviction**: passive on-read, no sweeper (homelab scale).
- **Prereleases**: dropped from the candidate set. Operators who want prerelease tracking opt into `Any` strategy.
- **Tag prefix `v`**: stripped before semver parse so `v1.2.3` and `1.2.3` rank equivalently.
- **Pagination cap**: 5 pages, 5000 tags. Beyond that, log a warning and use what we have.

## Open questions, deferred

- Should the cache TTL be configurable per `cycle_interval_seconds`? Defer; 1h is fine.
- Should we surface "tag bump available" in the dashboard before the approval is created? No; phase 9i UI ticket.
