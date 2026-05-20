HMAC-signed outbound webhooks for the Isengard controller.

The plugin runs controller-side and exposes two paths:

- **General webhooks.** Operator-configured rows in the `webhooks`
  table opt in to event kinds via a wildcard string
  (`*`, `update.success`, etc.). The [`subscriber`] task tails the bus
  and inserts a `webhook_deliveries` row per matching row per event.
- **Lifecycle hooks.** Per-container `container_hooks` rows carry
  `(pre_deploy_url, post_deploy_url, on_failure_url)`. The
  [`lifecycle`] task watches `deployment.*` bus events, looks up the
  matching hook rows, and enqueues lifecycle deliveries on the same
  table (`source = lifecycle`).

# Worker

[`worker::run`] ticks every `WORKER_TICK` (default 5s), claims up to
`WORKER_BATCH` pending due rows, and POSTs each one. Bodies carry
`Content-Type: application/json` and the HMAC signature header
[`sign::SIGNATURE_HEADER`]: `X-Isengard-Signature: sha256=<hex>`,
computed against the per-row secret.

Outcomes:

- **2xx.** `mark_delivery_success`.
- **4xx.** `mark_delivery_failed` (no retry: receiver says the
  request is permanently bad).
- **5xx or transport error.** `schedule_retry` per [`backoff::SCHEDULE`].
- **Attempts exhausted.** `mark_delivery_exhausted`.

# Backoff

[`backoff::SCHEDULE`] is `30s, 1m, 5m, 30m` between attempts; the cap
[`backoff::MAX_ATTEMPTS`] is `5`. The spec's `2h` slot is the wait
after attempt 5, which has no successor, so it's intentionally
omitted; bumping the cap to 6 reactivates it.

# Failure isolation

Each task gets its own `bus.subscribe()` so a lag on the general
subscriber doesn't drop lifecycle events (and vice versa). Per-row
send failures log at warn; the worker tick keeps draining the rest of
the batch.
