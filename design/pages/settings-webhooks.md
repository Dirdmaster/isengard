---
type: design
kind: page-spec
status: phase-12-pending
status_note: "Hooks & Webhooks is Phase 12"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
  - settings
  - webhooks
---

# Settings · Webhooks

Outbound webhooks: send events to external HTTP endpoints with HMAC signing.

Source design: [[Hooks & Webhooks]].

## Route

`/settings/webhooks`

## Sections

1. **Webhooks list** — table of configured outbound webhooks
   - Columns: NAME / URL / EVENTS / LAST DELIVERY / HEALTH (last N delivery success rate)
   - Row click → deliveries view
2. **+ Add webhook** — modal: name, URL, event filter (kind globs), HMAC secret (auto-generated), retry policy
3. **Lifecycle hooks** (read-only, summary) — count per stack with link to compose label docs

## Per-webhook deliveries view

Sub-page or expandable section showing:
- Stat cards: success rate (24h), p50/p95 latency, total deliveries
- Delivery table: TIME / EVENT / STATUS / LATENCY / RETRY COUNT
- Re-deliver button per row (manual replay)
- Test event button at top (sends synthetic event)

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Webhooks" cta="+ Add webhook" />`
- `<SettingsTabs active="webhooks" />`
- `<WebhooksTable />`
- `<WebhookEditor />` (modal)
- `<DeliveriesView />` — stats + table
- `<BottomBar />`

## States

- **Empty**: explainer + "+ Add your first webhook" with link to events docs
- **Healthy webhook**: green dot, "100% delivery rate"
- **Degraded** (recent failures, retries exhausting): amber + last error
- **Disabled** (manually paused or auto-paused after N failures): grey + Resume button
- **Test event flying**: spinner on row, success/fail badge appears

## Open questions

- ❓ Pause-after-N-failures threshold UI? — settings field, default 50 consecutive
- ❓ Per-webhook event filter syntax (globs vs regex)? — globs (kind matches `update.*`)
- ❓ Show outbound webhook payload schema docs inline? — link out to docs site

## Related

- Concepts: `concepts/2026-05-03-settings-webhooks-v1.html` (TODO)
- Source: [[Hooks & Webhooks]]

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
