# Phase 13 Plan A: Service detail page

A drill-down route for one service inside a stack. Closes the dead-end on the stack detail page where clicking a service today does nothing. Read-only metadata, last deployment, recent events scoped to the service, attached routing rules, effective policy, and a placeholder logs panel that points at Phase 13B.

Source design: `1 Projects/Isengard/Service Detail & Logs Streaming.md` and `design/pages/service-detail.md`.

Concept reference: `design/concepts/service-detail/v1.html`.

## Scope

In:
- New REST endpoint `GET /api/v1/services/:stack_id/:service_name`
- New SPA route `/stacks/[id]/services/[name]`
- Two-column layout: metadata (1fr) + logs panel + events (2fr)
- Effective policy preview reuse (`<EffectivePolicyPreview>` from 9d)
- Logs panel placeholder card linking to issue #57
- Quick actions cluster: Force update, Pause updates (deferred), Open routing settings
- ServiceChip click on stack overview routes to detail page

Out (deferred to 13B+):
- Live log streaming (Phase 13B, issue #57)
- Restart action wiring
- Exec shell action
- Per-host instance selector for multi-host services (Phase 13C)
- Env vars + volumes + networks panels (require agent introspection plumbing)

## Route

```
/stacks/[id]/services/[name]
```

Scope is `(stack_id, service_name)`. Service names are unique within a stack on a host. Multi-host fleets surface a status grid in 13C; for 13A we list per-host rows when a service exists across multiple hosts.

## REST endpoint

```
GET /api/v1/services/:stack_id/:service_name
```

Returns 200 with a JSON envelope or 404 when no service row matches both the stack and the name.

```ts
interface ServiceDetailDto {
  service: ServiceDto              // primary container info
  // When the same service name exists on multiple hosts under the same
  // stack name (fleet replicas), every other instance shows up here.
  other_instances: ServiceDto[]
  effective_policy: ResolvedPolicy
  last_deployment: DeploymentDto | null
  recent_events: EventDto[]        // last 50, filtered to this service
  routing_rules: RoutingRule[]     // fleet-wide rules attached to this service
}

interface ServiceDto {
  id: string                       // service primary key (i64 as string)
  host_id: string                  // ULID
  hostname: string                 // resolved from inventory for display
  stack_id: string | null
  name: string
  image: string
  state: 'running' | 'stopped' | 'restarting' | 'unknown'
  last_seen_at: string             // RFC3339
  deploy_strategy_override: string | null
}
```

The handler:

1. Loads the stack by id; 404 if absent.
2. Loads the service by `(host_id=stack.host_id, stack_id, name)`. 404 if absent.
3. Gathers `other_instances`: services with the same `name` whose stack name matches this stack's name on a different host. Empty in the single-host case.
4. Resolves the effective policy via `resolve_policy` with the host's fleet, the stack name, the service name, and the host id. No container label resolution in 13A: containers are not labeled service-side yet.
5. Picks the most recent deployment for the stack via `list_deployments_by_stack(stack_id, 1)` filtered to this service name. Falls back to `null` when the stack has not deployed yet.
6. Pulls 50 recent events from the journal, filters to this host id and either matching `container_name == service.name` or `container_name == None` events tied to the same host.
7. Pulls routing rules for the host, filtering to the service name.

Errors:
- 400 invalid stack_id (non-i64)
- 404 stack not found, service not found
- 500 storage failure (consistent with the rest of the dashboard API)

## UI layout

```
TopBar
PageHeader (breadcrumb: Stacks / <stack> / <service>, status pill)
  actions: Force update, Pause updates (disabled placeholder), Open routing
Body grid: 1fr / 2fr  (metadata left, logs+events right)
  Left column:
    METADATA card: scope (host + stack + service), image, state, last seen
    LABELS card: deploy strategy override (or "none"), routing rules count
    EFFECTIVE POLICY (collapsible <EffectivePolicyPreview>)
    LAST DEPLOYMENT card: state pill, started, finished, error (if any)
  Right column:
    LOGS PANEL (placeholder): dashed border card stating
      "Logs streaming arrives in Phase 13B (issue #57)"
    ROUTING RULES table (filtered to service)
    RECENT EVENTS list (last 50, scoped via API response)
    OTHER INSTANCES (only if non-empty): per-host rows with state pill
BottomStatusBar
```

Two-column proportions match the concept (`grid-cols-[1fr_2fr]`). On viewport <1200px the layout collapses to a single column with logs above events; the existing TailwindCSS responsive utilities cover that without bespoke media queries.

## Components

Reused:
- `<TopBar>`
- `<PageHeader>` (title + breadcrumb in slot, actions on the right)
- `<EffectivePolicyPreview>` (from policies/)
- `<EventRow>` (recent events list)
- `<KvRow>` (key-value rows in metadata cards)
- `<EmptyState>` (when service not found / no events / no routing rules)
- `<StatusPill>` (state)

New (this phase):
- `pages/stacks/[id]/services/[name].vue` (the route page)
- A small inline `<LogsPlaceholder>` block lives inside the page, since it disappears in 13B once the real panel ships.

## Drilldown wiring

`StackOverviewTab.vue` currently renders services as plain rows with an Expose button. Wrap the row in a `<NuxtLink>` to `/stacks/${stack.id}/services/${svc.name}` so clicking anywhere outside the inline action goes to the new page. The Expose button stays a click handler with `@click.stop` so it does not trigger navigation.

## Tests

REST tests in `crates/isengard-plugins/dashboard/tests/services_endpoints.rs`:

1. `service_detail_returns_404_when_stack_missing`
2. `service_detail_returns_404_when_service_missing`
3. `service_detail_single_host_returns_envelope`
4. `service_detail_includes_effective_policy`
5. `service_detail_multi_host_lists_other_instances`
6. `service_detail_includes_recent_routing_rules`

`bun run build` proves the SPA still type-checks.

## Out-of-scope clarifications

- The agent does not yet ship environment variables, volumes, or networks. Once an `inspect` heartbeat lands those fields land here verbatim. Until then the page deliberately omits those panels rather than mocking them.
- `Pause updates` is rendered as a disabled button with a tooltip pointing at issue #57's follow-up; the actual control belongs in the policy editor.
- `Restart` is not surfaced in 13A. Force update is the only action that already has a working endpoint (`/api/v1/stacks/:id/actions/force-update`); per-service force-update lands in 13H with the agent-side handler.

## Decisions

- **Logs placeholder**: a single dashed-border card with title "Logs" and body "Logs streaming arrives in Phase 13B (issue #57)" plus a link to the GitHub issue. No fake stream, no spinner, no skeleton.
- **Multi-host display**: list `other_instances` as a horizontally compact per-host table at the bottom of the right column. Each row shows host hostname, state pill, image. No host selector chip row in 13A; that ships when 13C wires aggregated views.
- **Routing rules scope**: filter on `host_id == service.host_id && service_name == name`. Fleet-wide replicas use the same name; this returns an exact-host match per service instance. Multi-host rendering uses `other_instances` to disambiguate.
