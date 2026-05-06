# Phase 9C: Discord interactive callbacks

Implements [[2026-05-06-phase-9c-discord-callbacks-design]]. Closes issue #45.

Branch: `feat/phase-9c`
Worktree: `~/Projects/isengard/.worktrees/phase-9c`
Base: `next` HEAD with Plan B (9e-9f) merged.

Implementer model: **Opus**.

## Standing self-review (every task)

1. `cargo build --workspace`
2. `cargo test --workspace` (or scoped fast iter; full workspace before committing)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Em dash (U+2014) and en dash (U+2013) scan over changed files: zero hits
6. `bun run build` (only if dashboard web bundle is touched; this slice does not change web)
7. Cite added/modified files in the per-task report

## Tasks

### T1: Notifier Discord interactive methods + subscriber

Files:
- `crates/isengard-plugins/notifier/Cargo.toml`: add `ed25519-dalek` (workspace dep new), `hex` (workspace dep new) only if storage doesn't already pull them. Likely add at workspace root.
- `Cargo.toml` (workspace): add `ed25519-dalek = "2.1"` and `hex = "0.4"` to `[workspace.dependencies]`.
- `crates/isengard-plugins/notifier/src/discord.rs`:
  - Keep `DiscordChannel` (one-way webhook) untouched.
  - New `DiscordInteractive` struct: bot token + channel id + api base + http client.
  - `from_env(channel_id, api_base) -> Option<Self>` reads `ISENGARD_DISCORD_BOT_TOKEN`; returns None if unset.
  - `send_action_row(text, buttons: &[Vec<InlineButton>]) -> Result<DiscordSentMessage>` POSTs to `/channels/<channel_id>/messages` with action row of buttons. Returns `(channel_id, message_id)`.
  - Free function `pub async fn edit_discord_message_text(api_base: Option<&str>, token: &str, channel_id: i64, message_id: i64, text: &str) -> Result<()>` performing the PATCH with `components: []`.
  - Free function `pub fn verify_discord_signature(public_key_hex: &str, timestamp: &[u8], body: &[u8], signature_hex: &str) -> Result<()>` reusable from the dashboard callback. Wraps ed25519-dalek.
- `crates/isengard-plugins/notifier/src/lib.rs`:
  - Read additional config: `discord.channel_id` (string-as-i64) for the interactive path. If absent, skip interactive setup.
  - Build `DiscordInteractive` if both Discord channel is present and `ISENGARD_DISCORD_BOT_TOKEN` is set. Hold as `discord_interactive: Option<Arc<DiscordInteractive>>`.
  - In the spawned dispatch task, on `update.pending_approval` events, call BOTH Telegram (if any) AND Discord (if any). Each persists its own metadata.
  - Settings warnings at init: if Discord config is present and `ISENGARD_DISCORD_BOT_TOKEN` is not set, log warn. If `ISENGARD_DISCORD_PUBLIC_KEY` is not set, log warn (interactive callbacks won't be authenticated).
- `crates/isengard-storage/src/host_action.rs`: add `set_discord_approval_message_metadata(action_id, channel_id, message_id)` mirroring `set_approval_message_metadata` but writing `notifier_discord_channel_id` + `notifier_discord_message_id`. Idempotent merge with existing metadata so Telegram values survive a Discord write and vice versa.

Commit: `feat(notifier): discord interactive messages + subscriber (refs #45)`

### T2: Dashboard Discord callback endpoint

Files:
- `crates/isengard-plugins/dashboard/Cargo.toml`: ensure `bytes` is available (likely transitive via axum) so the raw body can be extracted.
- `crates/isengard-plugins/dashboard/src/approvals.rs`:
  - New route `POST /notifier/callback/discord` mounted in `pub fn router(handles)`.
  - Extract `Bytes` (raw body) and `HeaderMap`. Pull `X-Signature-Ed25519`, `X-Signature-Timestamp`. Read `ISENGARD_DISCORD_PUBLIC_KEY` env.
  - Verify signature via `verify_discord_signature`. On any failure (missing header, missing env, bad signature, bad hex), respond 401.
  - Parse body as `DiscordInteraction`. Branch on `type`:
    - `1` -> respond `{ "type": 1 }`.
    - `3` -> extract `data.custom_id`, parse via existing `parse_callback_data`, resolve `decided_by` from `member.user.username` (guild context) or `user.username` (DM). Call shared decide path. Respond with UPDATE_MESSAGE shape (`type: 7`).
    - other -> 400.
  - On conflict (already decided), still respond 200 with UPDATE_MESSAGE referencing "Already decided".
  - Reuse `parse_callback_data`, `ParsedDecision`, `render_decided_message_text` from the existing module.
  - Best-effort `edit_discord_message_text` fallback: if the interaction did not carry `message.id` we look up `notifier_discord_*` metadata and edit out-of-band. (For type=3 the interaction carries `message.id` so this is mostly belt-and-suspenders.)
- Add envs to the constants block: `DISCORD_PUBLIC_KEY_ENV`, `DISCORD_BOT_TOKEN_ENV`, `DISCORD_API_BASE_ENV`, `DISCORD_SIGNATURE_HEADER`, `DISCORD_TIMESTAMP_HEADER`.

Commit: `feat(dashboard): discord callback endpoint with signature verify (refs #45)`

### T3: Tests

Files:
- `crates/isengard-plugins/notifier/tests/discord_interactive.rs` (new): 5+ tests
  - `send_action_row` payload shape (POST channels/<id>/messages, action row + 3 buttons, components type 1/2).
  - `edit_discord_message_text` shape (PATCH, components: []).
  - `send_action_row` 4xx surfaces error.
  - Subscriber-style: send + persist `notifier_discord_*` metadata via `Inventory`.
  - `verify_discord_signature` accepts a valid signature and rejects tampered ones.
- `crates/isengard-plugins/dashboard/tests/approvals_endpoints.rs` (extend): 4+ Discord tests
  - PING (type 1) with valid signature: 200 + `{"type": 1}`.
  - MESSAGE_COMPONENT (type 3) with valid signature + valid `custom_id`: state transitions, response is `{"type": 7, "data": {...}}`.
  - Bad signature: 401, state untouched.
  - Malformed `custom_id` (with valid signature): 400.
  - Process-global env mutation (`ISENGARD_DISCORD_PUBLIC_KEY`) goes through the existing `env_lock()` helper to avoid races with Telegram tests.
  - Helper `discord_signed(body) -> (headers, raw_bytes)` produces a valid signature for the test public key.

Commit: `test(notifier+dashboard): discord interactive coverage (refs #45)`

### T4: Spec + plan + design doc updates

Files:
- `docs/superpowers/specs/2026-05-06-phase-9c-discord-callbacks-design.md` (created above)
- `docs/superpowers/plans/2026-05-06-phase-9c-discord-callbacks.md` (this file)
- `design/pages/approvals.md`: bump `updated`, add a bullet "Discord callbacks shipped" under Implementation status.

Commit: `docs: phase 9c spec + plan + approvals page status (refs #45)`

### T5: Release notes

Files:
- `docs/RELEASE_NOTES_PHASE_9C.md` (new): operator-facing.
  - What's new: Discord interactive messages + callback endpoint.
  - Setup: register Discord application, bot token, public key, interactions endpoint URL, channel id config.
  - Webhook URL behavior unchanged for one-way fan-out.
  - Breaking: none.

Commit: `docs: phase 9c release notes (closes #45)`

### T6: Wrap-up + push + open PR

- Final gate sweep: cargo build, test, clippy, fmt, deny, bun build (no web changes so skip if untouched).
- `git push -u origin feat/phase-9c`
- `gh pr create --base next --title "feat: phase 9c (discord interactive callbacks)" --body "<see body>"` with "Closes #45" in the body.
- No merge.

## Execution order

T1 first. Then T2 (depends on T1 helpers). Then T3 (depends on T1+T2). Then T4 + T5 in parallel. Then T6.

## Final gates

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo deny check
```

All green = open PR, do not merge. Branch stays open for review.
