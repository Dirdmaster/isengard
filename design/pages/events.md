---
type: design
kind: page-spec
status: draft
status_note: "Live tail + search shipped; time range / Export JSONL / kind chip overhaul pending backend"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
  - observability
---

# Events

## Implementation status (2026-05-05)

- Shipped: live WS event feed, host dropdown filter, basic kind chips (UPDATED / FAILED / CHECKED / PULLING / DISCONNECT)
- Deferred: free-text search, time range picker, Export JSONL CTA, full `update.* / deploy.* / approval.* / agent.* / routing.*` chip set, virtualised list, live-tail pause banner, `⌘K events of <thing>` filter syntax
- Drift: row click routes to `/events/[id]` (a built-not-designed detail page) instead of jumping to the relevant entity (host/service/approval)


The fleet-wide event journal. Every state change, deploy, alert, approval, hook fire — chronological, filterable, jumpable.

## Audience

Operator post-incident: "what happened in the last hour and in what order?" Also live monitoring during a deploy: "are events flowing as expected?"

## Key interactions

- **Filter bar** — kind chips (update.* · deploy.* · approval.* · agent.* · routing.*), fleet, time range
- **Click event** — jumps to relevant entity (host, service, approval queue)
- **Live tail toggle** — top right, auto-scrolls as new events arrive (default ON)
- **Search** — free-text over event payloads
- **Export** — JSONL download of current filter (audit)
- **⌘K** — `events of <thing>` filters to that thing

## Components used

- `<TopBar />` (Events tab active)
- `<PageHeader title="Events" sub="<live>" cta="Export" />`
- `<EventFilterBar />` — kind chips + fleet picker + range + search
- `<EventList />` — virtualized timeline, newest at top
  - Each row: timestamp, kind chip, message, target link
- `<LiveTailToggle />`
- `<BottomBar />`

## States

- **Loading**: 8 skeleton rows
- **Empty** (zero events match filter): "No events match your filter. Try widening the range or clearing kind filters."
- **Populated streaming**: rows append from top with subtle highlight fade
- **Live tail paused** (user scrolled up): "12 new events" banner pinned to top of list, click resumes
- **Long range query** (> 24h): pagination footer, default 200/page
- **High burst** (deploy in progress): rows arrive fast, virtualized list keeps frame rate; rate indicator in PageHeader sub

## Open questions

- ❓ Severity colors (info / warn / error) on event chips? — yes, derive from kind prefix
- ❓ Group consecutive same-kind events? — defer to v1.x; show all for now
- ❓ Pinned filters (saved presets)? — defer
- ❓ Cross-fleet vs within-fleet by default? — within current fleet picker scope

## Related

- Concepts: `concepts/2026-05-02-events-v1.html`
- Implementation: builds on existing event journal + WS broadcast (Phase 4)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
