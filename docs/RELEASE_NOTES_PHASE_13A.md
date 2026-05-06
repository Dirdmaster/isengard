# Phase 13A: Service detail page

A drill-down page for one service inside a stack closes the stack-detail dead-end. Click any service row on `/stacks/<id>` and you land on the new page; it shows everything the controller knows about that container short of live logs.

## What works

- New route: `/stacks/:id/services/:name`.
- Two-column layout: metadata + effective policy + last deployment on the left, logs placeholder + routing + recent events + other-host instances on the right.
- Metadata card surfaces image, state, host, last-seen, and deploy strategy override.
- `<EffectivePolicyPreview>` is reused as a collapsible card so you can see exactly which scope contributed each field.
- Last deployment block shows strategy, state, started, finished, and the error string when a deployment failed.
- Recent events list filters journal rows to this service's host + container name.
- Routing rules attached to this service surface inline with hostname, target port, adapter, TLS mode, and state.
- Multi-host: when the same service exists across more than one host in a stack, the other instances appear in a compact per-host table at the bottom of the right column.
- Force update queues a stack-wide force update via the existing endpoint.
- Open routing jumps to Settings -> Networking.
- Stack overview rows are now `<NuxtLink>` wrappers; clicking anywhere outside the inline Expose action navigates to the new detail page.

## REST endpoint

```
GET /api/v1/services/:stack_id/:service_name
```

Returns a JSON envelope with `service`, `other_instances`, `effective_policy`, `last_deployment`, `recent_events`, and `routing_rules`. 404 when the stack or service does not exist. Six integration tests cover happy path, embedded policy, multi-host, attached routing rules, event filtering, and the two 404 paths.

## Deferred

The following items belong to follow-up phases:

- Live log streaming. The right column ships a dashed-border placeholder pointing at GitHub issue #57. Phase 13B wires the WebSocket + agent log tailing.
- Restart and Exec shell quick actions.
- Per-host instance selector chips (Phase 13C).
- Environment variables, volumes, networks panels (require an agent-side `inspect` heartbeat).
- Policy paused banner and deployment-in-progress card surface.
- ANSI rendering, in-buffer search, log download.

## Operator notes

- The page is read-only except for Force update. No operational risk to existing deployments.
- The new endpoint is a single round-trip per page mount; routing rules and recent events are not refetched continuously.
- The Pause updates button is rendered in a disabled state because the per-service policy editor that backs it is also pending; the tooltip points at the same follow-up issue.
