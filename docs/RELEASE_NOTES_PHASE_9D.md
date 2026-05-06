# Phase 9d: Maintenance Windows

Builds on the Phase 9a-9c policy foundation and the Phase 9e-9f approval flow. With this release, update policies can carry a maintenance window that constrains when the updater is allowed to apply changes. Outside the window, the cycle emits `update.deferred` with the next firing time and skips recreate.

Closes issue #46.

## What's new

- New `MaintenanceWindow` type on `Policy`: `{ cron_expr, timezone }`. Standard 5-field cron syntax. Timezone is optional (defaults to UTC).
- Resolver merges `window` per the existing layered scope precedence (global -> fleet -> stack -> service -> container). Provenance is tracked.
- Updater honors the window: outside-window candidates resolve to `PolicyDecision::Deferred { next_window }`, the cycle emits `update.deferred` with `next_window` (RFC3339, UTC), increments a `deferred` counter, and skips the recreate path. No approval row is persisted on the deferred path.
- New event kind `update.deferred`. Counter exposed in the existing `update.checked` cycle summary.
- REST: `POST /api/v1/policies` and `PUT /api/v1/policies/{scope_type}/{*scope_key}` validate the cron expression at write time. Malformed cron returns 400 with the parser message. Timezone validation stays lenient: the runtime falls back to UTC for unknown zones.
- UI: PolicyEditor gains a Window section with override checkbox, cron text input, IANA timezone dropdown (UTC, Europe/Zurich, America/New_York, Asia/Tokyo, custom), and a live "Next 3 firings" preview computed client-side. PolicyRow renders the window summary line. EffectivePolicyPreview includes a window row with provenance.

## Cron crate + tz handling

- `croner` 3.0 parses the cron expression. Accepts standard 5-field syntax; the UI helper text only documents 5-field but the parser also accepts 6-field with seconds.
- `chrono-tz` 0.10 resolves IANA timezone names. Unknown names log a warning and fall back to UTC.
- Window duration after a firing: 1h hard-coded. Configurable in a future release without a migration since the type is JSON-encoded.

## Setup steps

None. Existing policies without a `window` field continue to work unchanged (the field defaults to `None` via `#[serde(default)]`).

## Breaking changes

None. No migration. Backwards-compatible JSON deserialization for existing rows.

## Example: set update window to Sunday 02:00 only

1. Open Settings -> Policies and click "+ Add policy".
2. Pick scope: `fleet` with key `prod` (or any other scope).
3. Toggle "Override at this level" next to "Maintenance window".
4. Cron expression: `0 2 * * 0`
5. Timezone dropdown: Europe/Zurich.
6. Confirm the live preview shows the next 3 Sunday-02:00 firings in your chosen timezone.
7. Save.

From that moment on, the updater only applies the policy's covered services within the 1-hour window after each Sunday 02:00 Zurich firing. Outside that window, it emits `update.deferred` events with the next firing time, which the events feed and (in a future phase) the notifier surfaces.

## Backwards-compat verified

- `cargo test --workspace` clean
- Existing `PolicyRow.body_json` rows deserialize cleanly (test `policy_without_window_field_deserializes`)
- REST round-trip with valid window preserves cron + tz fields end-to-end (test `post_with_valid_window_round_trips`)

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9d.1 | Configurable window duration field on `MaintenanceWindow`. |
| 9g | Discord interactive callbacks (independent of this release). |
| 9i | `Minor` strategy semver-aware tag bumping. |
| 9j | Rollback failure handler. |
| 9b.1 | Container-label policy discovery from compose. |

## Notes

- An operator-approved update applies immediately, even outside the window. The window blocks the auto-apply path; the explicit approve path is a deliberate human override.
- The "Next 3 firings" preview uses a small client-side cron walker bounded to a 7-day horizon. Sparse patterns may show fewer than 3 firings; the server-side `croner`-backed validator is authoritative.
