---
type: design
kind: page-spec
status: stable
created: 2026-05-03
updated: 2026-05-03
tags:
  - design
  - page
  - settings
  - notifier
---

# Settings · Notifier

Configure outbound notification channels (Telegram, Discord, generic HTTP) and inbound callback endpoints (for approval flow).

## Route

`/settings/notifier`

## Sections

1. **Channels list** — Telegram / Discord / HTTP rows with config status, last message sent, test button
2. **+ Add channel** — picker: Telegram (bot token), Discord (webhook URL), HTTP (URL + auth header)
3. **Event subscription** — per-channel kind filters
4. **Approval callback URL** — auto-derived `controller://api/v1/notifier/callback/<provider>`; button to copy + test reachability from Telegram/Discord side
5. **Rate limits** — per-channel min interval between batched messages

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Notifier" cta="+ Add channel" />`
- `<SettingsTabs active="notifier" />`
- `<ChannelCard />` per channel with status + Test
- `<ChannelEditor />` (modal) — provider-specific form
- `<CallbackReachabilityCheck />` — banner when callback URL isn't reachable from outside
- `<BottomBar />`

## States

- **Empty**: explainer "Get notified about updates, approvals, and incidents" with channel picker
- **Channel healthy**: green dot, last message timestamp
- **Channel misconfigured** (bad token): red + reason
- **Callback unreachable**: amber banner with troubleshooting steps (cf-tunnel? raw NAT?) + fallback explainer (approval still works in dashboard)
- **Test message sent**: ephemeral toast "Sent ✓ — check your channel"

## Open questions

- ❓ Slack channel? — moved to v1.x per Context.md (Slack deprioritized in favor of Telegram/Discord)
- ❓ Per-fleet channel routing (prod alerts → ops channel, staging → dev channel)? — yes, channel cards take optional fleet filter
- ❓ Event throttling beyond rate limit (debounce identical events)? — yes, configurable

## Related

- Source: cross-cuts [[Update Policies & Approval Flow]] (callback flow is the approval gate)
- Concepts: (none yet)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
