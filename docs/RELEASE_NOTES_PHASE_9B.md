# Phase 9e-9f: Approval Flow + Telegram Callbacks

Builds on the Phase 9a-9d policy foundation. With this release, `gate=Approval` is honored end-to-end: the updater pauses the rollout, the controller persists a pending decision row, and the operator approves or rejects the update from the dashboard or directly from a Telegram message.

## What's new

- `gate=Approval` policy is now honored: the updater skips the update on resolve and persists a `pending_approval` row keyed on `(host_id, stack, service, proposed_digest)` so re-scans are idempotent.
- Dashboard `/approvals` queue page with filter chips (Open / Decided / All), per-row Approve / Reject / Snooze actions, and an in-TopBar `ApprovalsBadge` polled every 30s.
- Telegram interactive messages: outbound notifications now ship with an inline keyboard (Approve / Reject / Snooze) wired to a dedicated callback endpoint.
- Telegram callback handler (`POST /api/v1/notifier/callback/telegram`): verifies the bot secret, decides the pending action atomically, and edits the original message in place so the buttons disappear.
- Same approval flow callable from the dashboard (`POST /api/v1/approvals/:id`), with optimistic UI on the queue page.
- Snooze writes a service-scope `paused_until` via the Plan A policy upsert (read-modify-write so other field overrides survive); the next agent scan suppresses noise for that window.
- Lift of the Phase 9a `422` on `gate=approval` POST/PUT, since the gate is now enforceable.

## Setup steps

1. Set `ISENGARD_TELEGRAM_BOT_TOKEN` (already required by the existing notifier).
2. Set `ISENGARD_TELEGRAM_WEBHOOK_SECRET` (new). Any high-entropy string works; this is what Telegram echoes back as `secret_token` so the controller can authenticate inbound callbacks.
3. Register the webhook with Telegram once, pointing at your public controller URL:

   ```sh
   curl -X POST "https://api.telegram.org/bot<TOKEN>/setWebhook" \
     -d "url=<your-public-controller-url>/api/v1/notifier/callback/telegram&secret_token=<your-secret>"
   ```

   The controller's HTTP endpoint must be reachable from Telegram's servers. Cloudflare Tunnel works today; native cf-access integration is a later phase.

If `ISENGARD_TELEGRAM_WEBHOOK_SECRET` is unset, outbound messages still ship one-way and the controller logs a warning at startup.

## Breaking changes

None. Migration `0017` adds nullable columns to `host_actions` (`state`, `expires_at`, `decided_at`, `decided_by`, `metadata_json`, `action_id`); existing rows are untouched.

## How to use

1. Write a service-scope policy with `gate=approval` from Settings to Policies.
2. Push a new image to the registry.
3. On the next agent cycle, the controller persists a pending row and Telegram delivers a message with Approve / Reject / Snooze buttons. Tapping Approve dispatches the update; Snooze sets `paused_until` on the service-scope policy.

## Follow-ups (deferred)

| Phase | Summary |
|-------|---------|
| 9g | Discord interactive callbacks (same pattern as Telegram). |
| 9h | Maintenance windows (cron-like grammar for the `window` field). |
| 9i | `Minor` strategy: semver-aware tag bumping; will replace digest-pair rendering on the approval card with semver labels per concept v1. |
| 9j | Rollback failure handler (couples with Phase 10 deploy story). |
| 9b.1 | Container-label policy discovery from compose. |

## Notes

- Concept v1 rendered version diffs as semver labels (`v2.4.0 to v2.4.1`). The shipped card renders the digest pair (`sha256:0123abcd... to sha256:fedcba98...`) since the controller stores the proposed image digest, not a parsed semver. Tag-aware rendering is queued under Phase 9i.
- The Settings to Notifier "no webhook secret configured" hint banner was deferred since detection requires a server-side env exposure endpoint that does not exist yet. The startup warning covers the operator path for now.
