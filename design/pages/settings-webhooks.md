---
type: design
kind: page-spec
status: phase-12bc-implemented
status_note: "Outbound webhooks (12a) + lifecycle hooks (12b) + external-action gates (12c)"
created: 2026-05-03
updated: 2026-05-06
tags:
  - design
  - page
  - settings
  - webhooks
---

# Settings · Webhooks

## Implementation status

Phase 12a (#53) implemented:

- Outbound webhooks list view with status dot + per-row actions (Test, Deliveries, Enable/Disable, Delete)
- Add webhook modal with two-stage form: URL/secret/kinds/enabled, then a secret-flash step with Copy
- Per-webhook deliveries panel with status filter (pending/success/failed/exhausted)
- REST surface under `/api/v1/webhooks` (list/create/get/update/delete/deliveries/test)
- HMAC-SHA256 signing in `X-Isengard-Signature: sha256=<hex>`
- Persisted retry queue with 30s/1m/5m/30m backoff, max 5 attempts then `exhausted`

Phase 12b (#54) implemented (lifecycle hooks):

- Sub-tab "Lifecycle hooks" inside the Webhooks settings panel
- `isengard.hooks.pre_deploy|post_deploy|on_failure|secret` Docker labels parsed by the controller and stored in `container_hooks` keyed by `(host_id, container_name)`
- Lifecycle event subscriber on the webhooks plugin enqueues a `webhook_deliveries` row with `source='lifecycle'` and per-row URL+secret
- The 12a worker drains lifecycle rows alongside webhook rows (same retry policy, same HMAC header)
- `GET /api/v1/webhooks/deliveries?source=lifecycle` for the cross-webhook deliveries view

Phase 12c (#55) implemented (external-action gates):

- New "Gates" sub-tab; lists gate evaluations as deliveries with `source='gate'`
- New `external_gate` field on `Policy` (URL, optional secret, timeout_secs)
- `PolicyEditor.vue` gains an "External gate" section
- Updater consults the gate before applying any update; maps approve/reject/defer/manual JSON to the existing decision flow
- Failure modes per spec: timeout / 5xx / 4xx / parse-fail collapse to Manual; connection refused yields a 1h `paused_until` defer with `update.gated_unreachable`

Deferred (not in 12bc):

- Replay-a-specific-delivery button (deferred polish)
- Built-in destination templates (Slack/Discord/PagerDuty: Phase 12g)
- Auto-pause on sustained failures (Phase 12h)



Outbound webhooks: send events to external HTTP endpoints with HMAC signing.

Source design: [[Hooks & Webhooks]].

## Route

`/settings/webhooks`

## Sections

1. **Webhooks list** : table of configured outbound webhooks
   - Columns: NAME / URL / EVENTS / LAST DELIVERY / HEALTH (last N delivery success rate)
   - Row click → deliveries view
2. **+ Add webhook** : modal: name, URL, event filter (kind globs), HMAC secret (auto-generated), retry policy
3. **Lifecycle hooks** (read-only, summary) : count per stack with link to compose label docs

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
- `<DeliveriesView />` : stats + table
- `<BottomBar />`

## States

- **Empty**: explainer + "+ Add your first webhook" with link to events docs
- **Healthy webhook**: green dot, "100% delivery rate"
- **Degraded** (recent failures, retries exhausting): amber + last error
- **Disabled** (manually paused or auto-paused after N failures): grey + Resume button
- **Test event flying**: spinner on row, success/fail badge appears

## Open questions

- ❓ Pause-after-N-failures threshold UI? : settings field, default 50 consecutive
- ❓ Per-webhook event filter syntax (globs vs regex)? : globs (kind matches `update.*`)
- ❓ Show outbound webhook payload schema docs inline? : link out to docs site

## Related

- Concepts: `concepts/2026-05-03-settings-webhooks-v1.html` (TODO)
- Source: [[Hooks & Webhooks]]

---

> Approvals tab is pending Phase 9 : not currently in TopBar.
