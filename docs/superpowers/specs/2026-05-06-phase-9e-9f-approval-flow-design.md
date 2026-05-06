# Phase 9 Plan B (9e+9f): Approval Flow

Builds on Plan A (9a-9d) which shipped policies + Pinned/paused enforcement. This slice wires up `gate=Approval`: persist a pending decision when an update is detected, surface it in the dashboard + Telegram, dispatch the update on approve.

Scope:
- **9e**: gate=Approval enforcement + dashboard Approvals queue (in-dashboard only)
- **9f**: Telegram interactive messages + callback handler

Out of scope (deferred):
- 9g Discord interactive (same pattern as 9f, separate slice)
- 9h Maintenance windows (cron expressions)
- 9i Minor strategy (semver-aware tag bumping)
- 9j Rollback handler (couples with Phase 10)

## End-to-end flow

```
updater detects new digest
  -> resolve_policy(...)
  -> gate == Approval?
     -> persist pending_action(kind="update_pending_approval", body=UpdateApprovalBody{...}, expires_at=now+24h)
     -> emit update.pending_approval event
     -> stop (do not pull/recreate)

notifier subscribes to update.pending_approval
  -> render Telegram message with inline keyboard (Approve / Reject / Snooze 24h)
  -> store callback_data = action_id

operator taps button on phone
  -> Telegram POSTs to /api/v1/notifier/callback/telegram
  -> verify bot secret + parse callback_data -> action_id
  -> call decide_approval(action_id, decision)
  -> edit original message to show "Approved by @user at HH:MM"

OR operator opens dashboard /approvals
  -> sees list, clicks Approve / Reject
  -> POST /api/v1/approvals/:id with body {decision}
  -> same decide_approval(...) path

decide_approval:
  Approve  -> mark action approved, queue HostAction "apply_update", emit update.approved
  Reject   -> mark action rejected, emit update.rejected
  SnoozeHours(n) -> mark rejected, set service-scope policy paused_until = now+n hours, emit update.snoozed

agent sync stream picks up approved HostAction
  -> updater applies pull + recreate
  -> emits update.success or update.failed
```

## Storage

Extend existing `host_actions` table (no new migration needed beyond a CHECK relax if the `kind` column is constrained). Add a typed body for kind=`update_pending_approval`:

```rust
pub struct UpdateApprovalBody {
    pub host_id: HostId,
    pub stack: String,
    pub service: String,
    pub container_name: String,
    pub image: String,
    pub current_digest: String,
    pub proposed_digest: String,
    pub diff_url: Option<String>,        // GHCR-only in v1
    pub approver_channel: Option<String>,// from resolved policy
}
```

Storage DAO additions on `Inventory`:
- `insert_pending_approval(...)` returns the action id (ULID)
- `list_pending_approvals(filter: ApprovalFilter) -> Vec<PendingApprovalRow>`: filter by host_id / stack / state (open|approved|rejected|expired) / since
- `get_pending_approval(id) -> Option<PendingApprovalRow>`
- `decide_pending_approval(id, decision: ApprovalDecision, decided_by: &str) -> Result<DecidedApproval>`: atomic transition; returns the action so caller can dispatch
- `expire_pending_approvals(now)`: bulk transition `state=open AND expires_at < now -> expired`. Called from a periodic task in agent or controller.

State on `host_actions` for this kind: `pending_open`, `pending_approved`, `pending_rejected`, `pending_expired`, `pending_snoozed`. All under the existing `state` column, just new enum values.

12+ unit tests across DAO + body roundtrip + atomic decide.

## Updater integration

Building on Plan A's `policy::policy_decision`:

- Add a `PolicyDecision::PendingApproval(UpdateApprovalBody)` variant.
- When `gate=Approval` resolved, build the body from container metadata + resolver output, return that variant.
- In `do_cycle`, on PendingApproval: call `Inventory::insert_pending_approval(...)`, emit `update.pending_approval` event with the action_id, increment new `pending_approval` counter, continue (do not recreate).
- Idempotence: before persisting, check `list_pending_approvals(filter: open AND service AND proposed_digest match)`. If an open one already exists for the same proposed digest, skip. Avoids spamming notifier on every cycle.

3+ integration tests via the existing in-memory inventory harness: open approval persists once across cycles, dashboard list shows it, idempotence regression.

## REST: dashboard plugin

Mounted under existing `/api/v1`:

| Method | Path | Description |
|---|---|---|
| GET | `/approvals?state=&host_id=&stack=&since=` | List with filter; default `state=open`. |
| GET | `/approvals/:id` | Single. |
| POST | `/approvals/:id` | Body: `{ decision: "approve"\|"reject"\|"snooze", snooze_hours?: u32, decided_by?: string }`. Calls `decide_pending_approval` then if Approve dispatches `apply_update` HostAction. Returns updated row. |
| POST | `/notifier/callback/telegram` | Form/JSON body matching Telegram bot callback shape. Verifies `X-Telegram-Bot-Api-Secret-Token` header against `ISENGARD_TELEGRAM_WEBHOOK_SECRET`. Decodes `callback_data` -> `action_id` + `decision`. Calls same `decide_pending_approval` then edits message via Telegram bot API. |
| (PUT/POST policies) | lift the 422 returned for `gate=approval` from Plan A. | |

Validation:
- POST /approvals/:id with state != pending_open: 409 with current state message.
- POST /approvals/:id with snooze + missing snooze_hours: 400.
- Invalid decision: 422 with allowed values.

10+ endpoint tests.

## Dashboard UI

New route `/approvals`:
- Page lives at `web/pages/approvals.vue`
- New TopBar entry "Approvals" between "Events" and "Settings"; shows badge with open count from `usePendingApprovalsCount` composable (polled every 30s).
- Page body: list of `<ApprovalCard />` cards, filter chips (open / decided / all), refresh button.
- `<ApprovalCard />`: shows host . stack . service, image:current_digest -> proposed_digest, requested-at, expires-in (relative), Approve / Reject / Snooze buttons (Snooze opens a small dropdown with 6h/12h/24h/3d). Diff URL as inline link if present.
- Empty state: "No updates waiting on you. Next scan in <t>." (the `<t>` is best-effort; if too cute to compute, just "No pending approvals.").
- After action: optimistic update + refresh.

Sibling: `<ApprovalsBadge />` mounted in TopBar reading `usePendingApprovalsCount`.

## Notifier: Telegram interactive

Existing `crates/isengard-plugins/notifier/src/telegram.rs` currently sends one-way messages. Extensions:

- Subscribe to `update.pending_approval` event in the notifier plugin's main subscriber.
- Build inline keyboard:
  ```
  [✅ Approve] [❌ Reject] [⏰ Snooze 24h]
  ```
- `callback_data` for each button: `apv:<action_id>:approve`, `apv:<action_id>:reject`, `apv:<action_id>:snooze:24`. Keep under the 64-byte Telegram limit; ULIDs are 26 chars, so plenty of room.
- Send via existing `send_message` plumbing, augmenting with `reply_markup` field on the Telegram payload.
- Persist the sent message_id alongside the pending_approval row (new column `notifier_message_id` on `host_actions`, JSON metadata or dedicated migration; pick whichever's simpler — recommend JSON in the existing `metadata_json`).
- New `edit_message` helper: takes (chat_id, message_id, new_text, new_reply_markup). Used by the callback handler to mark a message as decided.
- Settings hint surfaced when `ISENGARD_TELEGRAM_WEBHOOK_SECRET` env unset: log a warning at controller startup, render a notice in Settings -> Notifier ("Set webhook secret to enable approval callbacks.").

Telegram webhook setup is the operator's job (run `setWebhook` once with their bot token + the controller URL + secret token). Document in release notes.

5+ tests around inline-keyboard payload, callback signature verification, edit_message wrapper. The actual Telegram round-trip is mocked.

## Open questions / decisions

- **Approver identity in callback**: Telegram tells us the callback `from.username`. Persist as `decided_by`. For dashboard decisions, hardcode `decided_by="dashboard"` until auth lands (cf-access in v1.x).
- **Snooze semantics**: when SnoozeHours is chosen, set `paused_until = now + N hours` on the *service-scope* policy (insert if absent). The next scan's resolver sees the pause and skips. Cleaner than re-emitting the same approval. The original approval action is marked rejected with reason="snoozed".
- **Race**: dashboard + Telegram both decide. First write wins (atomic `decide_pending_approval`). Second gets a 409 / "already decided" message.
- **Auto-expire**: the controller's existing periodic tick (heartbeat task or a new 60s tick) calls `expire_pending_approvals(now)` and emits `update.expired` for each.
- **Container-label policy override**: still deferred to 9b.1; the resolver respects them today if they exist, just no UI / discovery.

## Acceptance criteria

- [ ] `cargo build --workspace` clean; `cargo test --workspace` clean; `clippy -D warnings` clean; `fmt --check` clean; `cargo deny` clean
- [ ] `bun run build` (dashboard) clean
- [ ] `gate=approval` policy on a service results in an open pending_approval after a scan
- [ ] Approving via dashboard dispatches the update and the agent applies it
- [ ] Approving via Telegram (mocked callback in tests) does the same
- [ ] Snoozing for 24h pauses the service for 24h; next scan skips
- [ ] No em dashes in any new file (vault hard rule)

## Migration / breaking

- `host_actions.state` may need a CHECK relax for the new enum values. Migration 0017 if so.
- No data migration; existing actions unaffected.
- Operators must `setWebhook` once with their Telegram bot to enable callbacks. Document in release note.

## References

- Vault: [[Update Policies & Approval Flow]] (full design)
- Plan A spec: `docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md`
- Existing `host_actions` table: `crates/isengard-storage/src/host_action.rs`
- Existing notifier: `crates/isengard-plugins/notifier/src/telegram.rs`
