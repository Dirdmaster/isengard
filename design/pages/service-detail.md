---
type: design
kind: page-spec
status: stable
created: 2026-05-03
updated: 2026-05-03
tags:
  - design
  - page
  - logs
---

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
