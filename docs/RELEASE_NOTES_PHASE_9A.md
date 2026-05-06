# Phase 9a-9d: Update Policies (foundation)

The Update Policies foundation. Pin individual services, pause updates for a window, and scope policies at fleet, stack, or service granularity. Resolved policy is visible per service from the Stack detail page.

## What's new

- **Pin services**: set `strategy=Pinned` to freeze a service at its current image; the updater will skip it on every cycle and emit `update.policy_skipped`.
- **Pause updates**: set `paused_until=<RFC3339>` to suspend updates for a service until the timestamp passes.
- **Layered policies**: scope a policy at Global, Fleet, Stack, or Service. The resolver walks most-general to most-specific; per-field overrides are tracked with provenance.
- **Effective policy preview**: on a Stack page, every service shows its resolved policy with a provenance label per field (e.g. `inherited from FLEET: prod`).
- **Settings UI**: Settings to Policies lists every existing override row with scope label, gate badge, and the fields that override. The Policy editor modal uses field-level inheritance: each field shows the inherited value as placeholder, and an "Override at this level" checkbox activates the input.

## Breaking changes

None. Phase 9a-9d is purely additive:

- Services with no policy row resolve to the built-in defaults (`strategy=TagOnly`, `gate=Auto`), which matches pre-Phase-9 updater behavior.
- Existing deployments continue to update on the same schedule with the same semantics.

## Migration

Zero-touch. Migration `0016` runs automatically on controller start and creates the `policies` table. No data backfill is needed; absence of rows is the new default.

## How to use

Pin a service from the Settings page:

1. Open Settings to Policies, click `+ Add policy`.
2. Pick scope = Service, select the fleet / stack / service, set `strategy = Pinned`, save.
3. On the next updater cycle, the service is skipped and an `update.policy_skipped` event is recorded; the Stack page shows the policy effective with provenance `service` for the strategy field.

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9e | `gate=Approval` enforcement: updater holds, controller emits an approval request, user approves or denies via UI/notifier. |
| 9f | Notifier interactive messages: Slack / Discord / generic webhooks deliver approve/deny buttons. |
| 9g | Notifier interactive callbacks: inbound webhook routes from notifier to controller, signed and idempotent. |
| 9h | Maintenance windows: `window` field with cron-like grammar; updater respects it per service. |
| 9i | `Minor` strategy: semver-aware bumping, ignores major-version bumps until explicitly approved. |
| 9j | Rollback failure handler: couples with Phase 10 deploy semantics; on failed update, automatic revert to last good image. |
| 9b.1 | Container-label policy discovery: agents report container labels (`com.isengard.policy.*`), controller materializes implicit policy rows from them. |

## Notes

- `<EffectivePolicyPreview>` currently fetches each service's effective policy independently. A future optimization is to pass the parent stack's resolved policy down so the child only resolves the service-level delta. Left as a Phase 9 follow-up since it is a UX optimization, not a correctness issue.
- The PolicyEditor modal currently uses a generic title for all scopes. Tighter scope-aware titles (e.g. `Edit FLEET: prod policy`) are queued behind 9e UI work.
- Conflict detection (two rows for the same scope) is enforced by the storage UNIQUE constraint; the UI does not yet render a recovery banner because the constraint makes the case unreachable from the API. A defensive UI banner remains a follow-up.

See `docs/superpowers/specs/` for the Phase 9 spec and plan, and `design/pages/settings-policies.md` for the page-level design.
