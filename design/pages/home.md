---
type: design
kind: page-spec
status: draft
status_note: "StatRow shipped, but DeploysPanel/HealthAlert/+New dropdown deferred"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
---

# Home

## Implementation status (2026-05-05)

- Shipped: `<StatRow />` (hosts/stacks/approvals/deploys), TopBar with Home tab, post-wizard router redirect to `/welcome` when 0 hosts
- Deferred: `<DeploysPanel />` right column, `<HealthAlert />` degraded banner, `+ New ▾` dropdown (Add host / stack / routing rule)
- Drift: page header label says "Activity" while the TopBar tab says "Home" — left column is `StateStrip` + `EventTimeline` instead of the spec'd `<ActivityFeed limit="12" />` + 60/40 split


The dashboard root. Operational pulse: how is the fleet RIGHT NOW, what changed in the last hour, what needs attention.

## Audience

Returning operators opening Isengard for the daily check. They want a 10-second answer to "is anything broken?" with one click to drill into anything that is.

## Key interactions

- **Health glance** — top stat row (hosts up, stacks healthy, pending approvals, deploys in progress) is the primary scan target
- **Click a stat** — jumps to filtered view (`8 hosts up` → /hosts, `3 pending approvals` → /approvals)
- **Recent activity feed** — last 12 events. Click an event → relevant page (host, service, approval)
- **Active deploys panel** — when blue-green is running, surfaces inline progress per stack
- **+ New** dropdown (top right) — Add host / Add stack / Add routing rule
- **⌘K** — focus BottomBar cmd

## Components used

- `<TopBar />` (Home tab active)
- `<StatRow />` — 4 cells: hosts, stacks, approvals, deploys
- `<ActivityFeed limit="12" />` — left column, 60% width
- `<DeploysPanel />` — right column, 40% width, hidden when 0 active
- `<HealthAlert />` — top-of-body banner when any host unreachable >5m or any service crashlooping
- `<BottomBar />`

## States

- **Loading**: stat row shows numbers as skeleton; activity feed shows 6 shimmer rows
- **All-clear** (typical homelab): green stat row, "No active deploys" empty panel, calm activity feed
- **Has approvals**: amber pending-approvals cell with count + "Review now →" inline link
- **Active deploy**: deploys panel surfaces inline progress; activity feed mirrors the deploy events
- **Degraded** (1+ host unreachable, 1+ service crashlooping): red HealthAlert banner above stats, affected items linked
- **Empty fleet** (0 hosts after wizard skip): redirect to /welcome (handled by router)

## Open questions

- ❓ Stat row vs sparklines for trend? Stats give absolute count, sparklines give "is this going up". v1 = stats; v1.x = mini sparklines underneath
- ❓ How many activity events? 12 feels right for a desk-monitor view; mobile may want 6
- ❓ Pin-able cards? E.g., "always show prod fleet status here" — defer
- ❓ Customize layout? Move panels around? — out of scope, opinionated layout is the brand

## Related

- Concepts: `concepts/2026-05-02-home-v1.html`
- Implementation: `crates/isengard-plugins/dashboard/web/pages/index.vue`
- Cross: surfaces output from [[Update Policies & Approval Flow]] (approvals count) and [[Blue-Green Deployment]] (active deploys)

---

> Approvals tab is pending Phase 9 — not currently in TopBar. The pending-approvals stat in `<StatRow>` renders as `—` until the surface ships.
