# Phase 9 Plan B (9e+9f): Approval Flow

Implements [[2026-05-06-phase-9e-9f-approval-flow-design]] via subagent-driven workflow. Each task self-contained.

Branch: `feat/phase-9e-9f`
Worktree: `~/Projects/isengard/.worktrees/phase-9e-9f`
Base: `next` at HEAD with Plan A merged
Migration slot if needed: `0017`

Implementer model: **Opus** for every task.

## Standing self-review (every task)

1. `cargo build --workspace`
2. `cargo test --workspace` (or scoped to changed crates for fast iteration; full workspace before committing)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Em dash (U+2014) and en dash (U+2013) scan over changed files: zero hits
6. `bun run build` for dashboard tasks
7. Cite added/modified files + line counts in report

## Tasks

### T1: Storage extensions for pending approvals

**Goal**: extend `host_action.rs` to model approval rows + decision atomicity.

Files:
- `crates/isengard-storage/migrations/0017_host_action_approval_states.sql` (new) ONLY if existing `state` CHECK constraint blocks new enum values; otherwise skip migration. Inspect current constraint first.
- `crates/isengard-storage/src/host_action.rs` (extend): add `ApprovalState` enum, `UpdateApprovalBody` struct (serde-clean, snake_case for body_json compatibility), `ApprovalFilter` struct, `PendingApprovalRow` (transparent over HostActionRow with typed body), DAO methods:
  - `insert_pending_approval(InsertPendingApproval) -> Result<PendingApprovalRow>`
  - `get_pending_approval(action_id: &str) -> Result<Option<PendingApprovalRow>>`
  - `list_pending_approvals(ApprovalFilter) -> Result<Vec<PendingApprovalRow>>` ordered created_at DESC
  - `decide_pending_approval(action_id: &str, decision: ApprovalDecision, decided_by: &str) -> Result<DecidedApproval>`: `BEGIN ... UPDATE ... state=approved/rejected/snoozed, decided_at=now, decided_by=?, body_json=jsonb_set(metadata, ...) WHERE state='pending_open' AND id=? COMMIT`. Returns the row if updated, error if no rows affected (already decided).
  - `expire_pending_approvals(now: DateTime<Utc>) -> Result<Vec<PendingApprovalRow>>`: bulk update + return what got expired. Caller emits events.
  - `find_open_approval_for_proposed_digest(stack, service, host_id, digest) -> Result<Option<PendingApprovalRow>>`: idempotence check.
- `crates/isengard-storage/tests/host_action_approval.rs` (new): 12+ tests:
  - insert + get round-trip with body fields preserved
  - list filters by state, host_id, stack, since
  - decide approve transitions correctly
  - decide reject transitions correctly
  - decide snooze transitions correctly
  - decide rejects when state not pending_open (already decided)
  - decide returns the updated row
  - expire bulk transitions only past-expiry rows
  - idempotence query finds existing open row
  - body JSON roundtrip preserves diff_url None vs Some
  - `decided_by` persisted
  - decide is atomic under concurrent writes (best-effort: spawn 2 tasks racing; one wins)

**Hard constraints**: keep existing host_action callers (Phase 5d, 14, etc.) compiling. Do NOT rename existing fields or methods.

Commit: `feat(storage): pending_approval rows + atomic decide_pending_approval (T1 phase 9b)`

### T2: Updater integration

**Goal**: when resolved gate=Approval, persist a pending_approval (idempotently) and emit `update.pending_approval`. Skip the recreate.

Files:
- `crates/isengard-plugins/updater/src/policy.rs` (extend): new `PolicyDecision::PendingApproval(UpdateApprovalBody)` variant. `decision_from_resolved` returns it when `gate == Approval`.
- `crates/isengard-plugins/updater/src/lib.rs::do_cycle`: handle the new variant. Build `UpdateApprovalBody` from container metadata + resolver output (image, current_digest, proposed_digest from the registry-check step). Call `find_open_approval_for_proposed_digest` first; if exists, `pending_approval_dedup` counter++ and continue. Else `insert_pending_approval`, emit event `update.pending_approval` with action_id, increment `pending_approval` counter, continue.
- `crates/isengard-core/src/event.rs`: add `UPDATE_PENDING_APPROVAL`, `UPDATE_APPROVED`, `UPDATE_REJECTED`, `UPDATE_SNOOZED`, `UPDATE_EXPIRED` constants.
- `crates/isengard-core/src/policy_loader.rs`: any signature changes the updater needs (probably none).
- `crates/isengard-plugins/updater/Cargo.toml`: ensure ulid crate dep if the body needs it (likely already in storage).
- `crates/isengard-plugins/updater/tests/policy_approval.rs` (new): 4 integration tests:
  - approval-gated service persists a pending row on first cycle
  - second cycle does NOT create a duplicate (idempotence)
  - approving the action via DAO -> next cycle proceeds (recreate path called)
  - rejecting -> next cycle still skips (no new approval row created until digest changes)

Commit: `feat(updater): persist + emit pending_approval when gate=Approval (T2 phase 9b)`

### T3: Lift the 422 on policies POST/PUT

**Goal**: Plan A returned 422 if body.gate==approval. Now that the gate is honored, allow it.

Files:
- `crates/isengard-plugins/dashboard/src/policies.rs`: remove the gate==approval guard. Still validate the enum value is one of {auto, approval, never}.
- `crates/isengard-plugins/dashboard/tests/policies_endpoints.rs`: change the 422 test to assert 201/200 accepts approval gate.

Commit: `feat(dashboard): allow gate=approval on policy POST/PUT (T3 phase 9b)`

### T4: REST: /api/v1/approvals + decide flow + Telegram callback endpoint

Files:
- `crates/isengard-plugins/dashboard/src/approvals.rs` (new): handlers + DTOs + `pub fn router(...)` mounted under `/api/v1`.
  - GET `/approvals?state=open|decided|all&host_id=&stack=&since=`
  - GET `/approvals/:id`
  - POST `/approvals/:id` body `{ decision: "approve"|"reject"|"snooze", snooze_hours?: u32, decided_by?: string }`. Calls `decide_pending_approval`. If approve, dispatches `apply_update` HostAction via the existing pending-actions queue. If snooze, additionally writes a `paused_until = now + snooze_hours` on the service-scope policy (upsert).
  - POST `/notifier/callback/telegram`: verify `X-Telegram-Bot-Api-Secret-Token` header against env `ISENGARD_TELEGRAM_WEBHOOK_SECRET` (constant-time compare). Parse body as Telegram update; extract `callback_query.data`. Decode `apv:<id>:<decision>[:N]`. Dispatch to `decide_pending_approval`. Then call `telegram::edit_message_text` to flip the message to "Approved by @user at HH:MM".
- `crates/isengard-plugins/dashboard/src/lib.rs`: mount `approvals::router(handles.clone())`.
- `crates/isengard-plugins/dashboard/tests/approvals_endpoints.rs` (new): 10+ tests:
  - GET empty list
  - POST insert (via storage helper) + GET shows it
  - POST decide approve -> 200 + state pending_approved + pending_action queued (assert via storage)
  - POST decide reject -> state pending_rejected
  - POST decide snooze with 24h -> service-scope policy paused_until set
  - POST decide on already-decided -> 409
  - POST decide invalid value -> 422
  - Telegram callback with valid secret + valid data -> approves
  - Telegram callback with bad secret -> 401
  - Telegram callback with malformed callback_data -> 400

Commit: `feat(dashboard): /api/v1/approvals + telegram callback endpoint (T4 phase 9b)`

### T5: Dashboard UI (Approvals page + nav badge)

Files:
- `crates/isengard-plugins/dashboard/web/composables/useApprovals.ts` (new): list + decide methods.
- `crates/isengard-plugins/dashboard/web/composables/usePendingApprovalsCount.ts` (new): polled count for nav badge.
- `crates/isengard-plugins/dashboard/web/components/approvals/ApprovalCard.vue` (new)
- `crates/isengard-plugins/dashboard/web/pages/approvals.vue` (new): list view, filter chips, refresh.
- `crates/isengard-plugins/dashboard/web/components/TopBar.vue` (or wherever nav lives): add "Approvals" entry between Events and Settings; show count badge from composable.
- Empty state honors the in-container CTA rule.
- Loading + error states.

Acceptance:
- `bun run build` clean.
- Visual smoke: empty state, populated state, decision flow.

Commit: `feat(dashboard): approvals queue page + ApprovalCard + nav badge (T5 phase 9b)`

### T6: Telegram interactive messages

Files:
- `crates/isengard-plugins/notifier/src/telegram.rs` (extend):
  - `send_inline_keyboard(chat_id, text, buttons: Vec<Vec<InlineButton>>) -> Result<TelegramMessage>` - returns the sent message_id.
  - `edit_message_text(chat_id, message_id, text, reply_markup)` - used by the callback handler.
  - Public types InlineButton + TelegramMessage that the callback path can reuse.
- `crates/isengard-plugins/notifier/src/lib.rs`: subscribe to `update.pending_approval`. Build the keyboard, send, persist `notifier_message_id` + `notifier_chat_id` into the pending_approval action's `metadata_json` (use a tiny DAO helper `set_approval_message_metadata(action_id, chat_id, message_id)` added in T1 if it makes the seam cleaner; otherwise inline a generic `update_metadata` helper).
- Tests in `crates/isengard-plugins/notifier/tests/telegram_interactive.rs` (new): 5+ tests, all mocking the HTTP boundary:
  - send_inline_keyboard payload shape matches Telegram InlineKeyboardMarkup
  - edit_message_text payload shape correct
  - subscriber persists message_id on send success
  - bad_token / 4xx surfaces error not panic
  - settings warning logged when ISENGARD_TELEGRAM_WEBHOOK_SECRET unset

Commit: `feat(notifier): telegram inline keyboard + edit_message + approval subscriber (T6 phase 9b)`

### T7: Wiring + design status + release notes

Files:
- `design/pages/approvals.md`: change status to `shipped`, add Implementation status (2026-05-06).
- `design/pages/settings-policies.md` and `stack-detail.md`: cross-reference approvals page.
- `docs/RELEASE_NOTES_PHASE_9B.md` (new): operator-facing.
  - What's new: gate=Approval honored, Approvals queue, Telegram callbacks
  - Setup: `ISENGARD_TELEGRAM_WEBHOOK_SECRET` env, run `setWebhook` once
  - Breaking: none
  - Follow-ups: 9g (Discord), 9h (windows), 9i (Minor), 9j (Rollback)

Final gate sweep + commit + push branch + open PR against `next`. PR body summarizes shipped + deferred + setup steps. Do not merge.

Commit: `chore: phase 9e-9f wrap-up (design status + release notes)`

## Execution order

T1 first (storage). Then T2 + T3 in parallel. Then T4 + T6 in parallel (T6 needs T1's metadata helper). Then T5 (depends on T4). T7 last.

## Final gates

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo deny check
cd crates/isengard-plugins/dashboard/web && bun run build
```

All green = open PR, do not merge.
