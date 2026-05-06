# Phase 9e: Minor Strategy (semver-aware tag bumping)

Closes #47. Builds on the Phase 9a-9d policy foundation and the Phase 9e-9f approval flow. With this release, `strategy=Minor` is no longer a synonym for `TagOnly`: the updater consults the registry's tag list, picks the highest patch+minor on the current major, and proposes that as the next image. Auto and Approval gates apply unchanged.

## What's new

- `strategy=Minor` is enforced. When a service's resolved policy is `Minor`, every cycle:
  1. Lists the repository's tags via `GET /v2/<repo>/tags/list` (anonymous first, falls back to bearer/Basic auth via the existing docker-config credentials, paginated up to 5 pages / 5000 tags).
  2. Picks the highest semver-parseable tag whose major matches the running container, skipping prereleases (e.g. `1.3.0-rc.1`) and unparseable tags (e.g. `latest`, `nightly`).
  3. If a higher tag exists, HEADs that tag for its digest and proposes it as the update.
- Per-image, in-memory tag cache with 1h TTL. Concurrent cycles for the same image share an in-flight fetch via a per-key async mutex.
- Tag prefix `v` is preserved on the bumped image: a container at `v1.2.3` becomes `v1.3.0`, not `1.3.0`.

## Breaking changes

None. Phase 9e (Minor strategy) is purely additive:

- Services with no policy row, or with `strategy=TagOnly` / `Pinned` / `Any`, continue to behave as in Phase 9a-9d.
- Services already on the latest patch+minor see no change: the cycle resolves "no bump" and falls through to the normal `TagOnly` digest-only check.

## Migration

Zero-touch. No new tables, no new env vars. The cache is in-memory only.

## How to use

1. Open Settings to Policies, click `+ Add policy`.
2. Pick scope = Service (or Stack / Fleet for broader coverage), set `strategy = Minor`, save.
3. Push a new patch or minor tag for the service's image (e.g. `ghcr.io/foo/bar:1.3.0`).
4. On the next updater cycle, the proposed update targets the bumped tag. With `gate=Auto`, the recreate runs immediately. With `gate=Approval`, an approval row is persisted and surfaced via the dashboard / Telegram with the bumped image rendered as the `proposed_image`.

## Decisions

- **`semver` crate version**: `1.0` (current stable).
- **Cache eviction**: passive on-read (1h TTL). No background sweeper. Eviction is bounded by the live image set, which on a homelab is small.
- **Prereleases**: dropped from the candidate set. Operators who want prerelease tracking should opt into `Any`.
- **Tag prefix `v`**: stripped before semver parse so `v1.2.3` and `1.2.3` rank equivalently. The bumped tag is rebuilt with the same prefix as the running image.
- **Pagination cap**: 5 pages, 5000 tags. Beyond that the cycle logs a warning and uses the tags collected so far.
- **Failure mode**: any registry error during tag listing or HEAD on the bumped tag silently degrades to `TagOnly`. The next cycle retries.

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9g | Discord interactive callbacks for the approval card (mirrors 9f). |
| 9h | Maintenance windows (cron-like grammar for the `window` field). |
| 9i (UI) | Render the bumped tag pair (`v1.2.3` -> `v1.3.0`) on the approval card instead of the digest pair. The data is already threaded through. |
| 9j | Rollback failure handler (couples with Phase 10 deploy story). |
| 9b.1 | Container-label policy discovery from compose. |

## Notes

- The bumped image plus its digest are threaded through the existing `PendingApproval` body and the recreate path. No schema or API changes were required at the controller layer.
- The dashboard approval card still renders the digest pair (`sha256:0123abcd... -> sha256:fedcba98...`). Tag-aware rendering is a small follow-up that piggybacks on the `proposed_image` field already present in the approval body.
