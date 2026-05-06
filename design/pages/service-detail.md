---
type: design
kind: page-spec
status: partial
status_note: "Phase 13A + 13B shipped. 13A: route, two-column layout, metadata, effective policy, last deployment, recent events, routing rules, multi-host other_instances list. 13B: live logs streaming (WebSocket + agent bollard tail + multi-host fanout, pause/resume, host tabs, level + regex filter). Restart, Exec shell, per-host selector chip row, env vars / volumes / networks panels remain owed by 13C-13J."
created: 2026-05-03
updated: 2026-05-06
tags:
  - design
  - page
  - logs
---

## Implementation status (2026-05-06)

- Shipped (13A): `/stacks/[id]/services/[name]` route, `<PageHeader>` with breadcrumb + Force update + Open routing actions, two-column `grid-cols-[1fr_2fr]` body, metadata card (image/state/host/last-seen/deploy override), `<EffectivePolicyPreview>`, last deployment summary, recent events list, routing rules table, other-instances per-host list. Stack overview rows now drill in via `<NuxtLink>`.
- Shipped (13B): `<LogsPanel>` replaces the placeholder card. WebSocket endpoint `GET /api/v1/services/:stack_id/:service_name/logs/ws` opens an agent-side bollard `docker logs -f` tail per involved host (subscription_id-multiplexed over the existing Sync stream via new `StartLogStream` / `StopLogStream` ControllerMessages and a new `LogChunk` AgentMessage). Backfill, live tail, dropped-on-backpressure, unavailable-on-error, multi-host aggregation, host tabs, level + regex filter, pause/resume, last-N tail slider, hard-cap 5000 lines. 6 server-side integration tests + 4 agent unit tests + proto round-trips.
- Deferred (13C+): Restart and Exec shell actions, per-host instance selector chip row, env vars / volumes / networks panels (require agent introspection plumbing), policy paused banner, deployment-in-progress card, ANSI / search / download log polish.
- Drift: page does not yet ship a `<HostSelector />` chip row. Multi-host instances are shown as a compact per-host table at the bottom of the right column instead. The `<LogsPanel>` does ship per-host tabs internally so the chip row is not strictly needed for the logs use case.

# Service detail

The deepest leaf page. One container's full identity, live logs, and quick-action surface. The page operators land on after a Telegram alert.

Source design: [[Service Detail & Logs Streaming]] (full architecture).

## Route

`/stacks/[stack_id]/services/[service_name]`

When the service has multiple instances (replicas across hosts), the page surfaces a host selector chip row.

## Audience

Operator drilling from Stack detail or arriving via alert. They want logs streaming within ~1 second of page mount, with a clear path to Restart / Force update / Exec shell.

## Key interactions

- **PageHeader actions** — Restart · Force update · Expose · Exec shell (right side)
- **Host selector chips** — `[ all ▾ ]  prod-04  prod-05  prod-06` filter logs + status to one host
- **Log controls** — pause / resume / search / download (in the logs panel toolbar)
- **Routing card** — shows active rule + healthcheck status; Edit opens [[Networking & Proxy]] rule editor
- **Env panel** — secrets masked by default, click reveals (with audit event)

## Components used

- `<TopBar />`
- `<PageHeader />` with breadcrumb (Stacks/blog/web), action cluster
- `<HostSelector />` — chip row, `all` default
- `<MetadataPanel />` (left column ~40%): image+digest, status, started, health, routing card, env, volumes, networks
- `<LogsPanel />` (right column ~60%): live tail with virtualized list, toolbar, recent events strip below
- `<DeploymentProgressCard />` — only when service is mid-deploy
- `<PolicyBanner />` — only when service has paused_until or pending approval
- `<BottomBar />`

## States (key variants)

- **running healthy** (typical): green status, logs streaming, recent events show last successful update
- **deploying**: progress card pinned above metadata, logs prefixed with stage markers
- **stopped** (container exited): red status, last buffer shown with "container exited at HH:MM" marker, Restart enabled
- **container_not_found** (drift): yellow banner "container not found on this host", Refresh button forces agent rescan
- **agent disconnected**: red banner over logs panel, auto-reconnect 5 attempts then manual retry
- **multi-instance aggregated**: host selector = `all`, per-host status grid above logs, log lines prefixed with host id
- **policy paused**: amber banner explaining paused_until + Resume now (when permission allows)
- **logs unavailable** (no docker daemon, agent oom): logs panel shows error card with explanation
- **high volume throttle**: dropped-line markers inline `[N lines dropped — backpressure]`

## Open questions

- ❓ Vertical split vs horizontal logs panel on narrow screens? — vertical stack <1200px
- ❓ Persistent log filters across navigation? — store in URL hash for shareability
- ❓ Surface compose-time labels here (read-only)? — yes, in Metadata panel under "Labels"
- ❓ Force update bypassing approval = audit event with reason prompt? — yes, modal asks for short justification

## Related

- Concepts: `concepts/2026-05-02-service-detail-v1.html`
- Source: [[Service Detail & Logs Streaming]]
- Cross: [[Networking & Proxy]] (routing card), [[Update Policies & Approval Flow]] (policy banner), [[Blue-Green Deployment]] (deploy progress), [[Hooks & Webhooks]] (lifecycle hooks panel)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
