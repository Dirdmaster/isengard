# Phase 12 Plan A: Outbound Webhooks

Translates [[Hooks & Webhooks]] phase 12a (and folds in 12c+12d while we are here, since the work splits cleanly only at the UI seam) into a build-ready slice.

Vault source: `1 Projects/Isengard/Hooks & Webhooks.md`. Issue: #53.

Scope:
- **12a**: webhook plugin scaffold, storage table, outbound POST with HMAC signing
- **12c**: persisted retry queue with exponential backoff
- **12d**: deliveries history view (REST + UI)

Out of scope (deferred):
- 12b inbound test/replay UX polish (basic Test button only)
- 12e container lifecycle hooks (compose label parsing, agent-side)
- 12f external-action gate (synchronous decision points)
- 12g built-in templates (Slack, Discord, PagerDuty)
- 12h auto-pause on sustained failures
- JSONata payload templating

## End-to-end flow

```
controller emits Event on bus
  -> webhooks plugin subscriber receives
  -> for each enabled webhook whose event_kinds match:
       insert webhook_deliveries row (status=pending, attempts=0)
  -> delivery worker (5s tick) selects pending rows where next_retry_at IS NULL OR <= now
       compute body = canonical JSON of Event
       compute sig = HMAC-SHA256(secret, body) hex-encoded
       POST url with header X-Isengard-Signature: sha256=<hex>
       2xx -> status=success, last_attempt_at=now
       non-2xx or err -> attempts++, status=pending, schedule next_retry_at per backoff
       attempts >= 5 -> status=exhausted
```

Backoff schedule: 30s, 1m, 5m, 30m, 2h. Five attempts total. Hard cap.

## Storage

### Migration 0020

```sql
CREATE TABLE webhooks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    url          TEXT NOT NULL,
    secret       TEXT NOT NULL,
    event_kinds  TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id      INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event_kind      TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    last_error      TEXT,
    next_retry_at   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX idx_webhook_deliveries_status_retry
    ON webhook_deliveries(status, next_retry_at) WHERE status='pending';
```

`event_kinds` is a comma-separated list. Single value `*` means subscribe to every event. Match is case-sensitive exact string match per token.

`secret` is operator-provided (or auto-generated server-side). Stored as plaintext in the DB: the threat model is the same as for Telegram bot tokens (also plaintext). The dashboard shows it once on creation and never again on read.

`status` values: `pending`, `success`, `failed` (4xx, no retry), `exhausted` (>= 5 attempts).

## DAO

`WebhooksDao` lives on `Inventory`. Methods: `insert_webhook`, `get_webhook(id)`, `list_webhooks`, `update_webhook(id, body)`, `delete_webhook(id)`, `insert_delivery`, `list_deliveries(webhook_id, status_filter, limit)`, `claim_pending_deliveries(now, limit)`, `mark_delivery_success`, `mark_delivery_pending(attempts, next_retry_at, err)`, `mark_delivery_failed(err)`, `mark_delivery_exhausted(err)`.

`claim_pending_deliveries` is the worker's pull. It runs `SELECT id FROM webhook_deliveries WHERE status='pending' AND (next_retry_at IS NULL OR next_retry_at <= ?) ORDER BY id LIMIT ?` and returns full rows.

## Plugin: `isengard-plugin-webhooks`

Standalone crate (not a notifier extension): the surface is wide enough that mixing it into notifier would muddy `notifier`'s job (one-way fan-out to chat apps). Webhooks own their own subscriber, their own worker, their own DAO.

Two long-lived tasks spawned in `Plugin::start`:

1. **Subscriber**: tails the controller `EventBus`, on each event lists enabled webhooks, filters by `event_kinds`, inserts a `webhook_deliveries` row per match.
2. **Worker**: ticks every 5s, claims pending deliveries that are due, dispatches via `reqwest`, updates row state.

Failure isolation: a panic in either task gets logged; the other task survives. Both are abortable on `Plugin::stop`.

## REST endpoints (under `/api/v1`)

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/webhooks` | List all webhooks |
| POST | `/webhooks` | Create. Body: `{url, secret, eventKinds[], enabled}`. Returns row including secret (one-time read). |
| GET | `/webhooks/{id}` | Get one. Secret returned masked (last 4 chars). |
| PUT | `/webhooks/{id}` | Update url / kinds / enabled / secret. |
| DELETE | `/webhooks/{id}` | Delete (cascades deliveries). |
| GET | `/webhooks/{id}/deliveries?status=&limit=` | List deliveries for a webhook. |
| POST | `/webhooks/{id}/test` | Enqueue a synthetic `webhook.test` event. Returns the queued delivery row. |

Mounted by the dashboard plugin via `webhooks::router(handles)` analogous to policies/approvals/routing.

## Signature verification

`X-Isengard-Signature: sha256=<hex>` where `<hex>` is `hmac_sha256(secret_bytes, body_bytes)` lowercase. Body bytes are the raw POST body the receiver sees (no canonicalization beyond what `serde_json::to_string` produces). Receivers compute the same and `subtle::ConstantTimeEq` compare.

Crates: `hmac = "0.12"`, `sha2 = "0.10"` (already in workspace), `subtle = "2"` (already in workspace).

## UI: Settings -> Webhooks tab

`/settings?tab=webhooks` (added to `SettingsTabs`). Components:

- `WebhooksSettings.vue`: list view. Per-row: status dot (green if last delivery in 24h was 2xx, red if last failed/exhausted, grey if disabled), URL, event kinds (chips), action buttons (Test / View deliveries / Edit / Disable / Delete).
- `AddWebhookModal.vue`: URL, secret (auto-fill button), kinds multi-input, enabled toggle. On save, secret is shown once with a copy button and a warning that it will not be shown again.
- `WebhookDeliveriesPanel.vue`: table per webhook. Columns: time, kind, status, attempts, last error.

A `useWebhooks()` composable wraps the REST surface.

## Edge cases

| Scenario | Behavior |
|----------|----------|
| Receiver returns 4xx | Mark `failed`. No retry. |
| Receiver returns 5xx or unreachable | Mark pending, increment attempts, schedule next retry per backoff. |
| Receiver hangs | `reqwest` 10s timeout treated as 5xx. |
| Webhook disabled mid-flight | Pending deliveries continue to drain; new events skip the disabled row. |
| Delete webhook with pending deliveries | `ON DELETE CASCADE` removes them. |
| Event kinds list contains `*` | Match all. |
| event_kinds empty string | Match nothing. Operator probably wants `*`; UI defaults to `*`. |

## Decisions captured

- **Separate crate vs notifier extension**: separate crate. Notifier is a one-way chat fan-out; webhooks own a queue + retry + signing surface. Mixing them would force notifier to take a sqlx dep (it currently does not).
- **Retry schedule**: 30s, 1m, 5m, 30m, 2h. Front-loaded for transient blips; long tail for prolonged outages. Five attempts total covers ~2.5h before exhaustion.
- **Signature header**: `X-Isengard-Signature: sha256=<hex>`. Mirrors GitHub / Stripe convention so receivers can reuse existing verification snippets with one substitution.
- **Per-webhook secret**: operator sets it on create; dashboard shows once. No rotation flow in 12a (delete+recreate is the rotation story until 12h ships auto-pause).

## Cross-references

- Source: [[Hooks & Webhooks]] (vault)
- [[Update Policies & Approval Flow]]: events that webhooks subscribe to
- design/pages/settings-webhooks.md: page spec (gets `phase-12a-implemented` status)
