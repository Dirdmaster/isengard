Per-channel event rendering for the Isengard controller.

The notifier plugin runs controller-side. It subscribes to the
`ControllerHandles::bus` broadcast channel, filters events by kind, and
fans matches out to configured channels. Three channels ship in v1:
[`telegram`], [`discord`], and a generic [`http`] poster.

# Two paths through the dispatch loop

The runtime task in [`Notifier::start`] separates one-way fan-out from
interactive approvals:

- **Fan-out.** Every channel that registered interest in `event.kind`
  receives the event via its `send` impl. Default rendering goes
  through [`channel::format_event`] (multi-line plain text).
- **Interactive approval.** `update.pending_approval` events trigger
  inline-keyboard messages on Telegram (`handle_pending_approval`) and
  action-row buttons on Discord (`handle_pending_approval_discord`).
  Both record the resulting `(chat_id, message_id)` on the action row
  so the dashboard's callback handler can edit the message in place
  after a decision.

# Channel lifecycle

Each channel decodes its own config block under `[notifier.<name>]`:

- [`telegram::TelegramConfig`][] takes a bot token (env
  `ISENGARD_TELEGRAM_BOT_TOKEN` wins over config), chat ids, and an
  optional `api_base` for tests.
- [`discord::DiscordConfig`][] takes a webhook URL for one-way; an
  optional `channel_id` plus the env-sourced
  `ISENGARD_DISCORD_BOT_TOKEN` and `ISENGARD_DISCORD_PUBLIC_KEY`
  enable interactive callbacks.
- [`http::HttpConfig`][] takes a bare URL, optional headers, and an
  optional body template with `{{text}}` and `{{kind}}`.

Channels wrap behind [`channel::RateLimited`], a token-bucket limiter
with overflow batching. When the bucket runs dry, events queue and
flush on the next allowed send as one `notifier.batch` summary.

# Failure mode

Send failures log at warn and don't bubble out. The plugin's job is
best-effort observability: a flapping Discord webhook should not stall
the controller. Operators see failures in `isd ps notifier` logs.

# Smoke test

`isd notify test` (the CLI) emits a synthetic event into the bus and
expects each configured channel to deliver. See
`crates/isd/docs/help_groups.md` for the full operator surface.
