# Phase 9e: Minor Strategy (semver-aware tag bumping): implementation plan

Implementation plan for [[2026-05-06-phase-9e-minor-strategy-design]]. Branch: `feat/phase-9e`. Worktree: `~/Projects/isengard/.worktrees/phase-9e`. Base: `next` at `56f6d9a`. Closes issue #47.

## Standing self-review (every task)

Before declaring done:

1. `cargo build --workspace`
2. `cargo test --workspace` (or scoped to updater for plugin-only tasks)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Grep changed files for em dash (U+2014) and en dash (U+2013); zero tolerance
6. Cite exact files added/modified

## Task list

### T1: semver workspace dep + tag cache module skeleton

**Files**:
- `Cargo.toml` workspace deps: add `semver = "1.0"`
- `crates/isengard-plugins/updater/Cargo.toml`: add `semver.workspace = true`
- `crates/isengard-plugins/updater/src/tag_cache.rs` (new): `TagCache`, `CachedTags`, `pick_highest_minor`, `parse_tag`
- `crates/isengard-plugins/updater/src/lib.rs`: `pub mod tag_cache;`

**Acceptance**:
- `pick_highest_minor` covers: simple patch bump, simple minor bump, no-major-bump, prerelease drop, malformed-tag drop. 5 unit tests minimum.
- `parse_tag` strips one leading `v`.
- TagCache: 4 tests (cache hit returns same Arc, cache miss calls fetcher once, expiry triggers refetch, concurrent calls share fetch result).
- Public API uses `Arc<Vec<String>>` so the registry client can hand the cached slice through to callers without cloning.

### T2: list_tags on RegistryClient

**Files**:
- `crates/isengard-plugins/updater/src/registry.rs`: add `pub async fn list_tags(&self, image: &ImageRef) -> anyhow::Result<Vec<String>>` plus a private `tags_list_url` helper on `ImageRef`.
- `crates/isengard-plugins/updater/src/image_ref.rs`: add `pub fn tags_list_url(&self) -> String`.
- `crates/isengard-plugins/updater/Cargo.toml` dev-deps: `wiremock.workspace = true`.

**Acceptance**:
- 2+ wiremock tests: anonymous tags/list returns expected vec; paginated tags/list (Link header) aggregates pages.
- Same auth flow as `head_digest`: 401 -> bearer challenge -> retry. Internal helper extracted if it cleans up duplication; otherwise duplicate, since the auth dance is short.
- Hard pagination cap: 5 pages.

### T3: Updater integration

**Files**:
- `crates/isengard-plugins/updater/src/lib.rs`:
  - Add `tag_cache: Arc<TagCache>` to `Updater`.
  - In `do_cycle`, when `resolved.strategy == Minor` and the digest check finds no update, call `tag_cache.get_or_fetch(...)`, run `pick_highest_minor`, and if the bumped tag is strictly greater, HEAD the new tag for its digest.
  - Thread the bumped image + digest through `ApprovalContext` and the recreate path.
- Constants: `TAG_CACHE_DEFAULT_TTL_SECS = 3600`.

**Acceptance**:
- New end-to-end unit test in `lib.rs` (or new `tests/minor_strategy.rs`): with a faked registry / docker double, a `Minor` policy plus a registry that lists `[1.2.3, 1.2.4, 1.3.0, 2.0.0]` for a container running `1.2.3` triggers a recreate at `1.3.0`.
- Pre-existing `TagOnly`, `Pinned`, `Any` paths unchanged (regression coverage from existing tests stays green).

### T4: Wrap-up: design + release notes

**Files**:
- `design/pages/settings-policies.md`: move "Minor strategy" line from Deferred to Shipped; bump status_note.
- `docs/RELEASE_NOTES_PHASE_9E.md` (new): operator-facing notes (what's new, breaking changes, how to use, follow-ups).

**Acceptance**:
- Design page reflects shipped state.
- Release notes follow the Phase 9A/9B template.

### T5: Final gate sweep + PR

**Steps**:
1. `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
2. `git push -u origin feat/phase-9e`
3. `gh pr create --base next --title "feat: phase 9e (minor strategy)" --body "Closes #47..."`

**Acceptance**:
- All gates green on local.
- PR opened against `next`. Body references issue #47.

## Risk register

- Tag listing on Docker Hub for `library/<name>` requires the same `pull` scope as manifest fetch; the existing bearer-token logic already requests it. No additional auth work needed.
- Some private registries don't paginate via `Link`. The cap protects us; missing tags just mean a missed bump on this cycle.
- Semver crate's `Version::parse` rejects `latest`, `nightly`, etc. We swallow those silently: by design.

## Commit shape (small + per-task)

Each T should land as its own commit, prefixed `feat(phase-9e):` and referencing #47 in the body. Final PR squashes if needed. Workspace gates must be green before each commit.
