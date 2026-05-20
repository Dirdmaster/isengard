Interactive approval flow for `update.pending_approval` events.

When the updater plugin emits an `update.pending_approval` event, the
notifier turns it into an actionable message on whichever interactive
channels are configured. The flow:

1. Receive the event from the controller bus.
2. Decode `event.metadata` as a `PendingApprovalPayload` (action id,
   host id, stack, service, image, current digest, proposed digest).
3. Render channel-specific message text.
4. Send the message with inline buttons (`Approve`, `Reject`,
   `Snooze 24h`). The button payload is `apv:<action_id>:<verb>`.
5. Persist `(chat_id_or_channel_id, message_id)` on the storage row
   for `action_id`. The dashboard's callback handler reads these on
   button click and edits the message in place after recording the
   decision.

# Telegram specifics

- Sends to the first configured `chat_ids` entry. Multi-chat fan-out
  with a single decidable message would need richer tracking and lands
  later.
- Stores `chat_id` as `i64`. Numeric (negative for groups) parses
  fine; channel-style `@name` ids fall back to `0` with a warn log
  because the storage column is typed.
- Message body is HTML (`parse_mode=HTML`). Image digests get
  truncated to `sha256:<first 12 chars>` so the message stays compact.

# Discord specifics

- Requires both a configured `channel_id` (snowflake, parsed to `i64`)
  AND the `ISENGARD_DISCORD_BOT_TOKEN` env var. Webhook-only setups
  silently skip the interactive path with an info log.
- Action row carries three buttons; styles map by callback verb:
  `:approve` is green (style 3), `:reject` is red (style 4),
  everything else (snooze) is grey (style 2).
- Plain text content. Markdown would buy little and steal characters
  from the 2000-byte limit.

# Retry semantics

v1 has none. A failed send logs and bails. The updater's idempotence
check (`find_open_approval_for_proposed_digest`) keeps the next cycle
from creating duplicate rows; if the message metadata never gets
persisted, the action stays open and the operator decides via the
dashboard.

Revisit when a dedicated outbox earns its complexity.
