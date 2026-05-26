# Phase 12b + 12c: Lifecycle Hooks + External-Action Gates

Closes #54 (lifecycle hooks) and #55 (external-action gates). Builds on Phase 12a (#53) which shipped the outbound webhook foundation: `webhooks` + `webhook_deliveries` tables, the delivery worker, HMAC signing, and the Settings -> Webhooks UI.

## What is new

Phase 12b (container lifecycle hooks):

- New container labels parsed by the controller agent ingest:

  | Label                              | Meaning                                |
  |------------------------------------|----------------------------------------|
  | `isengard.hooks.pre_deploy`        | URL to POST before a deployment starts |
  | `isengard.hooks.post_deploy`       | URL to POST when a deployment finishes |
  | `isengard.hooks.on_failure`        | URL to POST on `aborted` or `failed`   |
  | `isengard.hooks.secret`            | Optional per-container HMAC secret     |

- New `container_hooks` table keyed by `(host_id, container_name)`. Populated by `HookLabelIngest` in parallel with the existing 9b.1 policy ingest.
- New `lifecycle` event subscriber on the `webhooks` plugin: tails `deployment.spinning_up`, `deployment.completed`, `deployment.aborted`, `deployment.failed` and enqueues a `webhook_deliveries` row per matching hook.
- The 12a delivery worker drains lifecycle deliveries alongside webhook deliveries with the same retry policy (5 attempts, 30s/1m/5m/30m/2h backoff).
- Settings -> Webhooks gains a "Lifecycle hooks" sub-tab listing recent lifecycle deliveries.

Phase 12c (external-action gates):

- New `external_gate` field on `Policy`:

  ```yaml
  external_gate:
    url: https://gate.example.com/decide
    secret: <optional HMAC secret>
    timeout_secs: 10
  ```

- Updater consults the gate URL BEFORE any update. The response JSON decides:

  | `decision` | Meaning                                                         |
  |------------|-----------------------------------------------------------------|
  | `approve`  | Proceed with the existing post-policy logic                     |
  | `reject`   | Skip update; emit `update.gated_reject`                         |
  | `defer`    | Set `paused_until` on the service; emit `update.gated_deferred` |
  | `manual`   | Escalate to the existing approval queue (Phase 9e flow)         |

- Failure modes:

  | Cause | Decision |
  |---|---|
  | HTTP 2xx parse OK | matching `GateDecision` |
  | HTTP 2xx parse fail | `Manual` (escalate) |
  | HTTP 4xx / 5xx | `Manual` |
  | Timeout (`timeout_secs`) | `Manual` |
  | Connection refused / DNS fail | `Unreachable` -> 1h `paused_until`; emits `update.gated_unreachable` |

- Settings -> Policies gains an "External gate" section in `PolicyEditor.vue`.
- Settings -> Webhooks gains a "Gates" sub-tab listing gate evaluations (each evaluation logged as `webhook_deliveries` with `source='gate'`).

## Storage

Migration `0021_lifecycle_hooks_and_gates.sql`:

- `webhook_deliveries.webhook_id` becomes nullable (recreated via shadow + INSERT-SELECT)
- New columns: `source TEXT NOT NULL DEFAULT 'webhook'` with `CHECK (source IN ('webhook','lifecycle','gate'))`, plus inline `url` and `secret` for non-`webhook` rows
- New table `container_hooks` with `UNIQUE(host_id, container_name)`

Existing 12a deliveries are migrated cleanly: every pre-existing row gets `source='webhook'` via the migration `INSERT ... SELECT` default.

## Operating model

The 12a worker reads URL+secret per delivery row:

- `source=webhook`  -> look up the parent `webhooks(id)` row (existing 12a path)
- `source=lifecycle` -> read `url`+`secret` from the row itself
- `source=gate`     -> read `url`+`secret` from the row itself

Lifecycle hooks fire-and-forget for v1: the spec's `required: true` "abort the deployment if the hook fails" mode is deferred to v2. A non-2xx lifecycle hook surfaces in `last_error` on the row but does not block the deployment.

External gates are synchronous: the cycle blocks for up to `timeout_secs` per evaluation. For fleets larger than ~50 services we will parallelise gate evaluations across the cycle in a later phase; the current sequential path is fine for the typical homelab + small-fleet workloads.

## Lifecycle hook receiver: Python example

```python
import hashlib
import hmac
from flask import Flask, request, abort

app = Flask(__name__)
SECRET = b"the-secret-from-isengard.hooks.secret"

@app.post("/pre-deploy")
def pre_deploy():
    sig = request.headers.get("X-Isengard-Signature", "")
    if not sig.startswith("sha256="):
        abort(400, "missing X-Isengard-Signature")
    expected = bytes.fromhex(sig[len("sha256="):])
    actual = hmac.new(SECRET, request.get_data(), hashlib.sha256).digest()
    if not hmac.compare_digest(expected, actual):
        abort(401, "bad signature")
    body = request.get_json()
    print(
        "pre-deploy",
        body["service"],
        body["blue_digest"], "->", body["green_digest"],
    )
    return "", 204
```

The same endpoint works for `post_deploy` and `on_failure` because the wire shape carries `kind` so the receiver can branch on it. Compose:

```yaml
services:
  web:
    image: ghcr.io/owner/repo:latest
    labels:
      isengard.hooks.pre_deploy: https://hooks.example.com/pre
      isengard.hooks.post_deploy: https://hooks.example.com/post
      isengard.hooks.on_failure: https://hooks.example.com/fail
      isengard.hooks.secret: the-secret-from-isengard.hooks.secret
```

## External-action gate receiver: Python example

```python
import hashlib
import hmac
from flask import Flask, request, abort, jsonify

app = Flask(__name__)
SECRET = b"the-secret-from-policy.external_gate.secret"

CHANGE_FREEZE_HOSTS = {"prod-04", "prod-05"}

@app.post("/decide")
def decide():
    sig = request.headers.get("X-Isengard-Signature", "")
    if SECRET and not sig.startswith("sha256="):
        abort(400, "missing signature")
    if SECRET:
        expected = bytes.fromhex(sig[len("sha256="):])
        actual = hmac.new(SECRET, request.get_data(), hashlib.sha256).digest()
        if not hmac.compare_digest(expected, actual):
            abort(401, "bad signature")

    body = request.get_json()
    if body["host_id"] in CHANGE_FREEZE_HOSTS:
        return jsonify({
            "decision": "defer",
            "until": "2026-06-01T00:00:00Z",
        })
    if body["service"] == "payments":
        return jsonify({"decision": "manual"})
    return jsonify({"decision": "approve"})
```

The `decide` callback receives a stable JSON payload:

```json
{
  "kind": "update.gate",
  "host_id": "01HX...",
  "stack": "blog",
  "service": "web",
  "container_name": "blog-web-1",
  "image": "ghcr.io/owner/repo:latest",
  "current_digest": "sha256:1111...",
  "proposed_digest": "sha256:2222...",
  "timestamp": "2026-05-06T12:00:00Z"
}
```

## Migration

There is no in-place migration step beyond starting the new binary; SQLite migration `0021` runs at controller boot. The pre-existing 12a webhook flow is regression-tested green: rows pre-migration land as `source='webhook'` automatically.

## Known limitations

- Per-row lifecycle / gate secrets are stored plaintext in `webhook_deliveries.secret` (same threat model as 12a's `webhooks.secret` column).
- `deployment.aborted` and `deployment.failed` both map to the `on_failure` hook; a single deployment that traverses both states fires twice. We accept this for v1 (the hook is fire-and-forget).
- No deduplication: a fleet that fans out 50 simultaneous deployments produces 50 lifecycle hook deliveries.
- Gate evaluation is sequential per cycle. Large fleets (~50+ services) will see cycle latency tied to gate response time. Parallelisation is a follow-up.
- Lifecycle hook payload field set is fixed; templating / JSONata transformation is deferred to Phase 12g.

Historical implementation notes are no longer kept in the public repository.
