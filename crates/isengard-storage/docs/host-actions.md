Queued action for a host, plus the pending-approval surface.

`host_actions` does two jobs.

# Agent-pull actions

The original purpose (migration `0006`). An operator queues an action
via `Inventory::queue_action`; the row sits with `delivered_at IS
NULL` until the agent calls `Inventory::pending_actions`, picks it
up, and the controller marks it delivered via
`Inventory::mark_action_delivered`. [`HostActionKind`] is the typed
shape of `payload_json`; today only [`HostActionKind::ForceUpdate`]
and [`HostActionKind::Decommission`] exist.

# Pending approvals

Migration `0017` reused the same table for approval rows. They share
the `(id, host_id, kind, payload_json, created_at, delivered_at,
result)` columns but add `action_id` (a ULID string used as the
external id), `state` (a `pending_*` enum), `expires_at`,
`decided_at`, `decided_by`, `metadata_json`, `updated_at`.

Approval rows set `delivered_at = CURRENT_TIMESTAMP` on insert so
they never bleed into the agent's `pending_actions` stream (which
filters `delivered_at IS NULL`). The agent doesn't see approvals;
they're operator-facing.

# Lifecycle

```text
pending_open -> pending_approved | pending_rejected | pending_snoozed
             -> pending_expired  (auto, when expires_at passes)
```

`Inventory::decide_pending_approval` is the atomic
`pending_open -> *` transition. Bulk auto-expiry runs through
`Inventory::expire_pending_approvals`: the controller's expiry task
calls it on a tick, gets the rows that flipped, and emits one
`update.expired` event per row.
