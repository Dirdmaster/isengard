# Phase 12a: Outbound Webhooks

Closes #53. Subscribes controller events to operator-defined HTTP endpoints with HMAC-SHA256 signing and retry on failure.

## What is new

- New crate `isengard-plugin-webhooks` (controller capability). Auto-loads via the existing plugin host.
- New tables `webhooks` + `webhook_deliveries` (migration `0020_webhooks.sql`).
- New REST surface under `/api/v1/webhooks`: list, create, get, update, delete, list deliveries, test.
- New Settings tab: Settings -> Webhooks. Add modal flashes the secret exactly once on creation.

## Operating model

A webhook subscribes to one or more event kinds (comma-separated, or `*` for everything). On each matching event the controller persists a row in `webhook_deliveries` and a worker drains the queue.

Each request carries:

```
POST <your-url>
Content-Type: application/json
X-Isengard-Signature: sha256=<hex>

<canonical Event JSON>
```

The signature is `HMAC-SHA256(secret_bytes, body_bytes)` lowercase hex.

Retry policy: 5 attempts max. Backoff between attempts: 30s, 1m, 5m, 30m. After the fifth failed attempt the row is marked `exhausted`. 4xx responses are treated as permanent failures (no retry, status `failed`). Non-2xx 5xx and network errors retry per backoff.

## Verifying signatures (Python receiver example)

```python
import hashlib
import hmac
from flask import Flask, request, abort

app = Flask(__name__)
SECRET = b"the-secret-the-dashboard-flashed-once"

@app.post("/isengard-hook")
def receive():
    sig = request.headers.get("X-Isengard-Signature", "")
    if not sig.startswith("sha256="):
        abort(400, "missing X-Isengard-Signature")
    expected_hex = sig[len("sha256="):]
    expected = bytes.fromhex(expected_hex)
    actual = hmac.new(SECRET, request.get_data(), hashlib.sha256).digest()
    if not hmac.compare_digest(expected, actual):
        abort(401, "bad signature")
    event = request.get_json()
    print("got event:", event["kind"], event.get("summary"))
    return "", 204
```

`hmac.compare_digest` is required: do NOT use `==` on the byte strings.

## Migration

There is no in-place migration step beyond starting the new binary; SQLite migration `0020` runs at controller boot.

## Known limitations

- Secrets are stored in plaintext in the controller DB (same threat model as the existing Telegram bot tokens).
- No payload templating (JSONata); receivers get the canonical `Event` JSON shape.
- No deduplication: 50 simultaneous failures fan out 50 deliveries per webhook.
- Replay-a-specific-delivery is not yet exposed in the UI; the Test button enqueues a synthetic `webhook.test` event but cannot replay an arbitrary historical delivery.
- No auto-pause after sustained failures (deferred to Phase 12h).
- Lifecycle hooks (compose labels, Phase 12e) and external-action gates (Phase 12f) are not part of this slice.

Historical implementation notes are no longer kept in the public repository.
