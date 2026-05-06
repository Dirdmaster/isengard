# Phase 9C: Discord interactive callbacks

Mirrors Phase 9f (Telegram) for Discord. Closes issue #45.

Plan B (9e-9f) shipped the approval queue + Telegram round-trip. Discord stayed one-way: webhook fan-out only. This slice ships the same end-to-end interactive flow on Discord using its Application + Interactions API.

## Scope

In:
- Discord interactive messages with action-row buttons on `update.pending_approval`.
- POST `/api/v1/notifier/callback/discord` endpoint with ed25519 signature verification.
- Stateless `edit_discord_message_text` helper used by the callback path.
- Subscriber registers Discord alongside Telegram when both are configured.
- Settings warnings when bot token / public key envs are missing.

Out:
- Slash commands or other Discord features beyond message components.
- Discord embeds (plain content + components only).
- Per-user permission gating beyond Discord's own role checks.

## Webhook vs bot decision

The existing one-way `discord` channel posts to an incoming webhook URL. Webhook URLs can deliver messages but cannot:
- Read interaction payloads
- Respond to interactions with the right ack shape
- Edit messages owned by the webhook by id without re-using the webhook URL

The Interactions API requires a registered Discord Application with a public key. The application either uses a bot token or relies on the registered application credentials to edit messages. Decision:

- Keep the existing webhook-based send path for one-way fan-out (`update.success`, `update.failed`, etc.). Unchanged.
- For interactive messages on `update.pending_approval`, require an additional bot token (`ISENGARD_DISCORD_BOT_TOKEN`) plus channel id (configured) and a public key (`ISENGARD_DISCORD_PUBLIC_KEY`).
- If those are not set, the controller logs a warning at startup and Discord falls back to one-way only. Telegram callbacks still work independently.

Rationale: webhook URLs do not carry the credentials needed to verify inbound interactions; mixing the two auth models keeps the simple webhook channel intact while letting interactive use cases opt in.

## Wire formats

### Discord components on send

```
{
  "content": "<text>",
  "components": [
    {
      "type": 1,
      "components": [
        { "type": 2, "style": 3, "label": "Approve", "custom_id": "apv:<action_id>:approve" },
        { "type": 2, "style": 4, "label": "Reject",  "custom_id": "apv:<action_id>:reject"  },
        { "type": 2, "style": 2, "label": "Snooze 24h", "custom_id": "apv:<action_id>:snooze:24" }
      ]
    }
  ]
}
```

`custom_id` mirrors Telegram's `callback_data`: `apv:<action_id>:<decision>[:hours]`. Discord caps `custom_id` at 100 bytes (more generous than Telegram's 64).

POST to `https://discord.com/api/v10/channels/<channel_id>/messages` with `Authorization: Bot <token>`. Response carries `id` (message id, snowflake i64) and `channel_id`. We persist both into the approval row metadata.

### Inbound interaction

Discord POSTs the registered Interactions Endpoint URL. Headers:
- `X-Signature-Ed25519`: hex-encoded 64-byte signature.
- `X-Signature-Timestamp`: ascii decimal seconds.

Signature is `ed25519_verify(public_key, timestamp || raw_body, signature)`. The raw body bytes matter: re-serializing JSON breaks the verify. We extract the raw body before parsing.

Interaction payload type values:
- `type=1` (PING): respond with `{ "type": 1 }` (PONG). Required for Discord to validate the endpoint at registration time.
- `type=3` (MESSAGE_COMPONENT): respond with `{ "type": 7, "data": { "content": "<edited text>", "components": [] } }` (UPDATE_MESSAGE) so the buttons disappear.

Other interaction types (slash commands, modal submits) are out of scope; respond 400.

### Editing messages

`edit_discord_message_text(token, channel_id, message_id, text)` performs:
```
PATCH /api/v10/channels/<channel_id>/messages/<message_id>
Authorization: Bot <token>
{ "content": "<text>", "components": [] }
```

Used by the dashboard callback (when no message_id is in the interaction payload) and could be used by future code (e.g., timeout sweepers).

## Notifier integration

`crates/isengard-plugins/notifier/src/discord.rs` extends with:
- `DiscordChannel` keeps its current outbound webhook role (no behavioural change).
- New `DiscordInteractive` struct holding bot token + channel id (and base URL override for tests). Constructed only if `ISENGARD_DISCORD_BOT_TOKEN` is set.
- `send_action_row(channel_id, text, buttons) -> DiscordSentMessage`
- Public `edit_discord_message_text(api_base, token, channel_id, message_id, text)` standalone helper for the dashboard path.

`lib.rs` Notifier:
- Build `DiscordInteractive` if both Discord channel is configured AND `ISENGARD_DISCORD_BOT_TOKEN` is set. Hold it as `Arc<DiscordInteractive>` alongside `telegram: Option<Arc<TelegramChannel>>`.
- In the spawned subscriber loop's `update.pending_approval` branch, fire Telegram (if any) AND Discord (if any). Each persists its own `(channel_id, message_id)` slot in the approval row metadata.
- Storage helper used: existing `set_approval_message_metadata` for Telegram (writes `notifier_chat_id` + `notifier_message_id`); a new `set_discord_approval_message_metadata` for Discord (writes `notifier_discord_channel_id` + `notifier_discord_message_id`) so they coexist when both are wired up.

## Dashboard endpoint

`crates/isengard-plugins/dashboard/src/approvals.rs` adds:

```
POST /api/v1/notifier/callback/discord
```

Flow:
1. Read `X-Signature-Ed25519` + `X-Signature-Timestamp` headers; require both.
2. Read raw request body bytes (axum `Bytes` extractor).
3. Verify with `ed25519-dalek`'s `VerifyingKey::verify`. On any failure, respond 401.
4. Parse body as `DiscordInteraction`.
5. If `type == 1` (PING): respond `{"type": 1}` (PONG). 200 OK.
6. If `type == 3` (MESSAGE_COMPONENT): extract `data.custom_id`, parse with the existing `parse_callback_data` (shared with Telegram). Resolve `decided_by` from `member.user.username` or `user.username`. Apply same decision path. Respond with `{"type": 7, "data": {"content": "<text>", "components": []}}` (UPDATE_MESSAGE) so Discord swaps the buttons out for the decided text.
7. If `type` is anything else: 400.
8. On already-decided conflict: respond 200 with UPDATE_MESSAGE saying "Already decided: <reason>".

Public key sourced from env `ISENGARD_DISCORD_PUBLIC_KEY` (hex-encoded 32 bytes). Bot token (used for the optional out-of-band edit fallback) from `ISENGARD_DISCORD_BOT_TOKEN`. API base override env `ISENGARD_DISCORD_API_BASE` for tests.

## Tests

Notifier (`tests/discord_interactive.rs`, 5+):
- `send_action_row` payload shape (action row + 3 buttons) hits POST `/channels/<id>/messages`.
- `edit_discord_message_text` issues PATCH with `components: []`.
- Bad token / 4xx surfaces error not panic.
- Subscriber persists `notifier_discord_channel_id` + `notifier_discord_message_id` after a successful send (uses real in-memory inventory).
- `verify_signature` accepts a real signed request (using a test keypair) and rejects tampered ones.

Dashboard (`tests/approvals_endpoints.rs` extensions, 4+):
- PING (type 1) with valid signature: returns `{"type":1}`.
- MESSAGE_COMPONENT with valid signature + valid `custom_id`: state transitions to `pending_approved`, responds with UPDATE_MESSAGE.
- Bad signature: 401.
- Malformed `custom_id`: 400 (with valid signature so we hit the parse branch).
- Conflict (already decided): 200 with UPDATE_MESSAGE carrying "Already decided".

Tests serialize ed25519 sign keys via `rand` once per test scope; signatures are produced by `SigningKey::sign(timestamp || body)`.

## Edge cases

| Case | Behavior |
|---|---|
| `ISENGARD_DISCORD_PUBLIC_KEY` unset | All inbound callbacks 401 with log line. PING discovery fails until set. |
| `ISENGARD_DISCORD_BOT_TOKEN` unset | Outbound interactive sends are skipped at startup with a warn log; one-way fan-out unchanged. |
| Both Telegram AND Discord wired for the same approval | Each persists its own metadata key pair; whichever decides first wins via the atomic `decide_pending_approval`; the other can still read the row but the second decide call returns 409. |
| Discord rate-limits outbound (HTTP 429) | Surfaces as Err from `send_action_row`; subscriber logs a warn and bails. The action stays open (operator can decide via dashboard). |
| Empty channel id in config | `from_config` returns Err, plugin init fails. |
| User clicks the button after the action expired | Decide path returns 409 conflict; endpoint responds with UPDATE_MESSAGE "Already decided: expired". |

## Acceptance criteria

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` clean (new + existing)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo deny check` clean (ed25519-dalek + hex license review)
- [ ] `bun run build` (dashboard) clean
- [ ] No em dashes (U+2014) or en dashes (U+2013) in any new file
- [ ] `update.pending_approval` with Discord configured produces an interactive message
- [ ] Decided via Discord button: state flips, edit_message swaps content, force_update queued
- [ ] Public key unset: all inbound callbacks 401

## References

- Phase 9e-9f spec: `docs/superpowers/specs/2026-05-06-phase-9e-9f-approval-flow-design.md`
- Phase 9e-9f release notes: `docs/RELEASE_NOTES_PHASE_9B.md`
- Vault: [[Update Policies & Approval Flow]]
- Discord Interactions API: https://discord.com/developers/docs/interactions/receiving-and-responding
