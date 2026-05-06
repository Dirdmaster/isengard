# Phase 12b + 12c: Container Lifecycle Hooks and External-Action Gates

Translates [[Hooks & Webhooks]] phases 12b (lifecycle hooks) and 12c (external gates), folded into a single shippable slice. Phase 12a (#53) shipped the storage + delivery worker foundation; this slice adds the event sources that emit into it plus the synchronous gate variant.

Vault source: `1 Projects/Isengard/Hooks & Webhooks.md`. Issues: #54 + #55.

## Scope

Phase 12b (lifecycle hooks):
- New container labels `isengard.hooks.pre_deploy|post_deploy|on_failure` (URL each)
- New label `isengard.hooks.secret` (per-container HMAC override)
- Controller-side label ingest into a new `container_hooks` table
- Lifecycle subscriber on the `webhooks` plugin that listens to `deployment.spinning_up`, `deployment.completed`, `deployment.aborted`, `deployment.failed`
- For each event, look up hooks for `(host_id, container_name)` of the deployment's blue and green containers; enqueue a `webhook_deliveries` row with `webhook_id = NULL`, `source = 'lifecycle'`
- The existing 12a worker drains those rows; HMAC signing reuses `compute_signature`

Phase 12c (external gates):
- New `Policy.external_gate: Option<ExternalGate>` field with `{ url, secret, timeout_secs }`
- New `evaluate_gate` async function in the updater plugin (HTTP POST, HMAC sign, parse, default-on-failure)
- Updater integration: when the resolved policy carries an `external_gate`, evaluate before `decision_from_resolved` runs, mapping the response to existing decisions
- Optional `webhook_deliveries` log row per gate evaluation with `source = 'gate'` so operators see gate traffic in the same audit surface

Out of scope:
- Inbound callback for async (defer-then-callback) gate responses: gates are synchronous
- Lifecycle hooks `required: true` semantics (abort on hook failure): all lifecycle hooks fire-and-forget for v1
- WASM hooks (option B in the design doc, deferred to v2)
- Built-in templates (12g)
- Auto-pause on sustained failures (12h)

## End-to-end flow

### Lifecycle hooks

```
agent emits ContainerLabelsReport (existing wire shape)
  -> controller HookLabelIngest parses isengard.hooks.* labels
  -> upserts container_hooks row keyed by (host_id, container_name)

agent driver emits deployment.spinning_up / completed / aborted / failed
  -> webhooks plugin lifecycle subscriber receives
  -> looks up container_hooks for the deployment's blue + green container names
  -> for each matching kind (pre_deploy / post_deploy / on_failure), enqueues
     a webhook_deliveries row with webhook_id=NULL, source='lifecycle',
     url + secret captured into a new payload column on that row
  -> existing 12a worker drains; signature uses the captured per-row secret
```

### External gates

```
updater cycle finds candidate with needs_update=true
  -> resolves policy
  -> if resolved.external_gate.is_some():
       evaluate_gate(gate, payload).await
       on Approve  -> Proceed
       on Reject   -> emit update.gated_reject; skip
       on Defer(t) -> upsert paused_until=t at service-scope policy; emit update.gated_deferred
       on Manual   -> persist PendingApproval row + emit update.pending_approval (existing 9e flow)
       on transport / parse error -> Manual (default)
       on URL unreachable          -> emit update.gated_unreachable; treat as Defer(now+1h)
       on signature verify failure -> log + treat as Reject
```

## Storage

### Migration 0021

```sql
-- 12b/c: lifecycle hook deliveries + gate deliveries share the 12a
-- webhook_deliveries table. webhook_id becomes NULL-able and a new
-- discriminator column tags the source.

ALTER TABLE webhook_deliveries ADD COLUMN source TEXT NOT NULL DEFAULT 'webhook';
ALTER TABLE webhook_deliveries ADD COLUMN url TEXT;
ALTER TABLE webhook_deliveries ADD COLUMN secret TEXT;

-- SQLite cannot ALTER a NOT NULL away in place. Recreate the table by
-- copying through a temp shadow.

CREATE TABLE webhook_deliveries_v2 (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id      INTEGER REFERENCES webhooks(id) ON DELETE CASCADE,
    source          TEXT NOT NULL DEFAULT 'webhook'
                    CHECK (source IN ('webhook','lifecycle','gate')),
    url             TEXT,
    secret          TEXT,
    event_kind      TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    last_error      TEXT,
    next_retry_at   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

INSERT INTO webhook_deliveries_v2
  (id, webhook_id, source, url, secret, event_kind, payload_json,
   status, attempts, last_attempt_at, last_error, next_retry_at, created_at)
SELECT id, webhook_id, source, url, secret, event_kind, payload_json,
       status, attempts, last_attempt_at, last_error, next_retry_at, created_at
FROM webhook_deliveries;

DROP TABLE webhook_deliveries;
ALTER TABLE webhook_deliveries_v2 RENAME TO webhook_deliveries;

-- Recreate indexes
CREATE INDEX idx_webhook_deliveries_status_retry
    ON webhook_deliveries(status, next_retry_at) WHERE status='pending';
CREATE INDEX idx_webhook_deliveries_source_created
    ON webhook_deliveries(source, created_at DESC);

-- Hook configuration table: one row per (host, container_name).
CREATE TABLE container_hooks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB NOT NULL,
    container_id    TEXT NOT NULL,
    container_name  TEXT NOT NULL,
    pre_deploy_url  TEXT,
    post_deploy_url TEXT,
    on_failure_url  TEXT,
    secret          TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE(host_id, container_name)
);

CREATE INDEX idx_container_hooks_host_container ON container_hooks(host_id, container_id);
```

### DAO additions

Webhook deliveries:
- `insert_lifecycle_delivery(host_id, container, kind, url, secret, payload_json) -> WebhookDelivery`
- `insert_gate_delivery(url, secret, payload_json, decision_outcome) -> WebhookDelivery`
- `list_deliveries_by_source(source: DeliverySource, limit: i64) -> Vec<WebhookDelivery>`
- `WebhookDelivery` gains `source: DeliverySource`, `url: Option<String>`, `secret: Option<String>`

Container hooks:
- `upsert_container_hooks(host_id, container_id, container_name, pre, post, on_failure, secret) -> ContainerHooks`
- `delete_container_hooks(host_id, container_name) -> bool`
- `get_container_hooks(host_id, container_name) -> Option<ContainerHooks>`
- `list_container_hooks_by_host(host_id) -> Vec<ContainerHooks>`

The 12a worker logic stays untouched: it reads `webhook_id` to fetch the URL+secret for `source='webhook'`, but for `source='lifecycle'|'gate'` it now reads the URL+secret from the row's own columns.

## Hook label format

```yaml
services:
  web:
    labels:
      isengard.hooks.pre_deploy: https://hooks.example.com/pre
      isengard.hooks.post_deploy: https://hooks.example.com/post
      isengard.hooks.on_failure: https://hooks.example.com/fail
      isengard.hooks.secret: my-shared-secret  # optional
```

Empty / unset URL means "no hook for this event". An invalid URL (parse failure) is logged at warn and stored as-is; the delivery worker will surface the parse error in `last_error` when it tries to send.

## Lifecycle subscriber

Lives in `isengard-plugins/webhooks/src/lifecycle.rs`. Distinct broadcast subscriber from the 12a `subscriber.rs`: uses the same `bus_rx` clone and the same `Inventory`, but only acts on deployment.* events.

```rust
pub async fn run(inventory: Arc<Inventory>, mut rx: Receiver<Event>) {
    while let Ok(event) = rx.recv().await {
        if let Some(kind) = lifecycle_kind(&event.kind) {
            let _ = on_lifecycle(&inventory, &event, kind).await;
        }
    }
}

fn lifecycle_kind(s: &str) -> Option<HookKind> {
    match s {
        "deployment.spinning_up" => Some(HookKind::PreDeploy),
        "deployment.completed"   => Some(HookKind::PostDeploy),
        "deployment.aborted"     => Some(HookKind::OnFailure),
        "deployment.failed"      => Some(HookKind::OnFailure),
        _ => None,
    }
}
```

The deployment driver already serializes the full Deployment row into `event.metadata.deployment`, so the subscriber pulls `host_id`, `service_name`, `green_container`, `blue_container`, `green_digest`, `blue_digest` from there. For each container name (green and blue, when present), it loads the matching `container_hooks` row. If a row has the URL for the matching kind, it inserts a lifecycle delivery.

## External-gate evaluator

Lives in `isengard-plugins/updater/src/gate.rs` so the pure resolver in `isengard-core` stays HTTP-free. The pure types (`ExternalGate`, `GateDecision`, `GatePayload`) live in `isengard-core::policy::gate`.

```rust
pub struct ExternalGate {
    pub url: String,
    pub secret: Option<String>,
    pub timeout_secs: u32,
}

pub enum GateDecision {
    Approve,
    Reject { reason: Option<String> },
    Defer { until: DateTime<Utc> },
    Manual,
    Unreachable,  // signal the caller to take the unreachable code path
}

pub struct GatePayload {
    pub kind: &'static str, // "update.gate"
    pub host_id: String,
    pub stack: String,
    pub service: String,
    pub container_name: String,
    pub image: String,
    pub current_digest: String,
    pub proposed_digest: String,
    pub timestamp: DateTime<Utc>,
}

pub async fn evaluate_gate(http: &Client, gate: &ExternalGate, payload: &GatePayload) -> GateDecision;
```

Failure modes:

| Cause | Decision |
|---|---|
| HTTP 2xx with `{"decision":"approve"}` | Approve |
| HTTP 2xx with `{"decision":"reject","reason":...}` | Reject |
| HTTP 2xx with `{"decision":"defer","until":...}` | Defer |
| HTTP 2xx with `{"decision":"manual"}` | Manual |
| HTTP 2xx with malformed body | Manual |
| HTTP 4xx | Manual |
| HTTP 5xx | Manual |
| Timeout (gate.timeout_secs) | Manual |
| Connection refused / DNS failure | Unreachable |

The unreachable case is distinct because callers want to emit a different event (`update.gated_unreachable`) and apply a 1h backoff via paused_until rather than escalate to a human.

## Updater integration

Inside the cycle's `policy_decision` flow:

```rust
let resolved = resolve_policy(&projected, ctx);
if let Some(gate) = &resolved.external_gate {
    let payload = build_gate_payload(ctx, &approval_ctx);
    let log_row_id = inventory.insert_gate_delivery(...).await?;
    let decision = evaluate_gate(&http, gate, &payload).await;
    inventory.update_gate_delivery_outcome(log_row_id, &decision).await?;
    match decision {
        GateDecision::Approve => /* fall through to the rest */,
        GateDecision::Reject { reason } => {
            emit("update.gated_reject", reason);
            return Skip(GateRejected);
        }
        GateDecision::Defer { until } => {
            inventory.upsert_policy_paused_until(service_scope, until).await?;
            emit("update.gated_deferred", until);
            return Deferred { next_window: Some(until) };
        }
        GateDecision::Manual => {
            // Same code path as gate=Approval today.
            return PendingApproval(build_pending_approval_body(...));
        }
        GateDecision::Unreachable => {
            let until = Utc::now() + Duration::hours(1);
            inventory.upsert_policy_paused_until(service_scope, until).await?;
            emit("update.gated_unreachable", until);
            return Deferred { next_window: Some(until) };
        }
    }
}
```

Gate evaluation runs AFTER pinned/paused/window checks and BEFORE the existing approval path: a paused service shouldn't even consult the gate, but the gate is given a chance to `Manual` an otherwise-Auto service.

## REST + UI

`PUT /api/v1/policies` body gains an `externalGate` field. `PolicyEditor.vue` gains an "External gate" section (URL textarea, optional secret with masking, timeout numeric).

`WebhooksSettings.vue` gains a sub-tab toggle: "All sources" / "Webhooks" / "Lifecycle" / "Gates". The "Lifecycle" and "Gates" tabs reuse `WebhookDeliveriesPanel` but call `GET /api/v1/webhooks/deliveries?source=lifecycle` (a new endpoint) instead of the per-webhook list.

## Testing

| Surface | Test count | Coverage |
|---|---|---|
| Migration 0021 + DAO | 6 | source CHECK constraint, webhook_id nullable, lifecycle/gate insert helpers, list_deliveries_by_source, ContainerHooks CRUD |
| Lifecycle subscriber | 5 | matching kind enqueues, missing hook row noop, all-four event kinds map correctly, blue + green both queried, malformed metadata noop |
| Gate evaluator | 6 | wiremock 200 each decision, 5xx -> Manual, timeout -> Manual, malformed body -> Manual, connection refused -> Unreachable, signature header sent |
| Updater integration | 4 | Approve falls through, Reject skips, Defer sets paused_until, Manual escalates |
| Dashboard policy schema | 2 | external_gate round-trips, REST PUT accepts |
| Dashboard webhooks deliveries | 2 | source filter returns only matching rows, default no filter returns all |

Total: 25.

## Backwards compatibility

- Phase 12a webhook deliveries continue working unchanged: existing rows get `source='webhook'` via the migration default; new rows default to `'webhook'` when inserted via `insert_delivery`.
- Existing `Policy` rows decoded from JSON missing the `external_gate` key produce `external_gate=None` (serde default). Round-trip test included.
- The 12a worker reads URL+secret for `source='webhook'` rows by joining to the `webhooks` table (existing path); for `source='lifecycle'|'gate'` rows it reads them from the row itself. This is a small worker change but does NOT touch the wire format the existing webhook receivers see.
