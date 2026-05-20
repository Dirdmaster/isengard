Outbound webhook DAOs.

Migration `0020` lands `webhooks` and `webhook_deliveries`; migration
`0021` adds the [`DeliverySource`] discriminator so the same delivery
table can absorb container-lifecycle hooks and policy gate
evaluations.

# Three delivery shapes

[`DeliverySource::Webhook`] is the original 12a shape. Rows reference
a `webhooks(id)` row; the worker dispatches to the parent row's
`url + secret`.

[`DeliverySource::Lifecycle`] (12b) is for container hook fires. The
parent row is a `container_hooks` row, but the FK lives on the labels
not on `webhook_deliveries`, so URL and secret are carried inline.

[`DeliverySource::Gate`] (12c) records one external-action gate
evaluation. URL and secret are inline for the same reason as
lifecycle.

# Delivery state machine

```text
pending -> success
        -> failed       (terminal: 4xx, no retry)
        -> exhausted    (terminal: retry cap hit)
        -> pending      (transient failure, next_retry_at set)
```

`Inventory::claim_pending_deliveries` is what the worker calls on
its dispatch loop; it returns rows whose `next_retry_at <= now`. The
worker writes the outcome via one of the `mark_delivery_*` paths.

# Kind matching

[`kind_matches`] is the shared helper webhook rows use to filter
events. The filter is a comma-separated list; `*` matches everything;
whitespace is trimmed.
