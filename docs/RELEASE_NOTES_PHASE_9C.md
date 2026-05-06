# Phase 9c: Discord Interactive Callbacks

Phase 9c brings the same interactive approval flow that 9f shipped for Telegram to Discord. Operators with a Discord-only workflow can now Approve / Reject / Snooze updates directly from the chat surface; the controller verifies Discord's ed25519 signature on every callback.

## What's new

- Discord interactive messages on `update.pending_approval`: a plain-text body plus an action row of buttons (Approve / Reject / Snooze 24h). Discord's components v2 wire format; `custom_id` matches Telegram's `apv:<action_id>:<decision>[:hours]`.
- New endpoint `POST /api/v1/notifier/callback/discord`. Verifies the ed25519 signature over `timestamp || raw_body` against `ISENGARD_DISCORD_PUBLIC_KEY`. PING (type 1) returns PONG so Discord can validate the endpoint at registration time. MESSAGE_COMPONENT (type 3) parses the `custom_id`, runs the same `decide_pending_approval` path the dashboard and Telegram callbacks use, and responds with UPDATE_MESSAGE (type 7) so Discord clears the buttons in place.
- Notifier subscribes to `update.pending_approval` for Discord alongside Telegram. Both fire independently; each persists its own `(channel_id, message_id)` pair on the approval row metadata so they coexist when both surfaces are wired up.
- Storage: new `set_discord_approval_message_metadata(action_id, channel_id, message_id)` mirroring the Telegram helper.

## Setup steps

The webhook URL you used for one-way Discord notifications still works untouched. The interactive path needs a Discord Application + bot token (webhook URLs cannot read interactions).

1. Create a Discord application at https://discord.com/developers/applications. Note the Application ID and the Public Key from the General Information page.
2. Under "Bot", reset the token and copy it. Under "OAuth2 -> URL Generator", scope `bot` + `applications.commands`, and invite the bot to your server with at least the `Send Messages` permission.
3. Find the channel id (Discord client: enable Developer Mode, right-click the channel, "Copy Channel ID"). Numeric snowflake.
4. Set environment variables on the controller:

   ```sh
   export ISENGARD_DISCORD_BOT_TOKEN="<bot token from step 2>"
   export ISENGARD_DISCORD_PUBLIC_KEY="<public key from step 1>"
   # optional: override the API base for staging or air-gapped tests
   # export ISENGARD_DISCORD_API_BASE="https://discord.com/api/v10"
   ```

5. Add the channel id to the notifier config under `discord.channel_id`. The existing `webhook_url` field stays exactly as before for one-way fan-out:

   ```json
   {
     "discord": {
       "webhook_url": "https://discord.com/api/webhooks/<id>/<token>",
       "channel_id": "1234567890",
       "kinds": ["update.success", "update.failed"]
     }
   }
   ```

6. In the Discord developer portal, set the Interactions Endpoint URL to `<your-public-controller-url>/api/v1/notifier/callback/discord`. Discord will POST a PING immediately to validate the URL; the controller responds with PONG when `ISENGARD_DISCORD_PUBLIC_KEY` matches.

If `ISENGARD_DISCORD_BOT_TOKEN` is unset, Discord stays one-way and the controller logs a warning at startup. If the bot token is set but `ISENGARD_DISCORD_PUBLIC_KEY` is missing, outbound interactive messages still ship but inbound button clicks are rejected as unauthenticated.

## Breaking changes

None. The webhook URL path is unchanged. The new `discord.channel_id` config field is optional; existing configs continue to fan out one-way without the interactive path.

## How to use

1. Wire up Discord per the setup above.
2. Set a service or fleet policy with `gate=approval`.
3. Push a new image. On the next agent cycle, the controller fans `update.pending_approval` to Telegram (if configured) and Discord (if configured).
4. Tap Approve in Discord. The bot edits the message in place ("Approved by discord:@you at HH:MM UTC (apply_update queued)") and the agent picks up the dispatched `force_update` HostAction on its next sync.
5. Already-decided clicks return UPDATE_MESSAGE with "Already decided: <reason>" so the user gets feedback rather than a silent failure.

## Operator gaps and follow-ups

- The dashboard does not yet expose a "Test interactions endpoint" button. Use Discord's built-in PING at registration time to confirm reachability.
- "No notifier configured" banner detection still requires the env-exposure endpoint that has not been built; the same gap noted in 9f release notes applies here.
- Slash commands and modal submits are out of scope; the endpoint returns 400 for non-PING / non-MESSAGE_COMPONENT interactions.

## Related issues

- Closes #45 (Phase 9c).
