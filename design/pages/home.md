---
type: design
kind: page-spec
status: shipped
status_note: "v1.html surface re-shipped (rebuild from richer concept); Pencil divergence ADR'd"
created: 2026-05-03
updated: 2026-05-07
tags:
  - design
  - page
---

# Home

## Implementation status (2026-05-07)

- Shipped:
  - `<StatRow />` (HOSTS / STACKS / APPROVALS / DEPLOYS) with click-through to filtered views
  - `<ActivityCard />` (last 12 events, flat reverse-chrono, no day separators, "View all" link)
  - `<ActiveDeploysCard />` (in-flight blue/green rollouts, progress bar, empty state)
  - `<HealthSnapshotCard />` (hosts up, stale agents, fleet roll-up, last event, "All systems operational" empty state)
  - Page header (22px sans title + relative-time subtitle "last updated …")
  - TopBar (search bar + ⌘K kbd + "+ New ▾" dropdown for Add host / routing rule)
  - Inspector right rail (340px) renders selected event detail; sits beside the right column cards, not replacing them
  - Post-wizard router redirect to `/welcome` when 0 hosts
- Deferred (still in v1.html, not yet wired):
  - Sparkline inside the DEPLOYS stat cell (skipped: BottomStatusBar already has a 12-bar 24h event sparkline; adding a second one would duplicate signal)
  - "↑ N today" delta on HOSTS shows when hosts were enrolled in the last 24h, but the worst-host status dot uses the simpler "all reporting / some stale" heuristic until a richer `host.health` field exists
- Diverges from `design/app.pen` (the Pencil-locked simpler v3 layout): see [[2026-05-07-home-rebuild-from-v1-overrides-pencil]]

The dashboard root. Operational pulse: how is the fleet RIGHT NOW, what changed in the last hour, what needs attention.

## Audience

Returning operators opening Isengard for the daily check. They want a 10-second answer to "is anything broken?" with one click to drill into anything that is.

## Key interactions

- **Health glance**: top stat row (hosts, stacks, approvals, deploys) is the primary scan target
- **Click a stat**: jumps to filtered view (HOSTS card to /hosts, APPROVALS card to /approvals, DEPLOYS card to /stacks)
- **Recent activity feed**: last 12 events. Click an event to populate the right rail Inspector
- **Active deploys card**: when blue-green is running, surfaces inline progress per stack
- **Health snapshot card**: hosts reporting, stale agents, fleet distribution, last-event time
- **Inspector right rail**: 340px fixed, persistent. Empty state when nothing selected
- **+ New** dropdown (top right): Add host / Add stack (soon) / Add routing rule
- **⌘K**: focus BottomStatusBar cmd

## Components used

- `<TopBar />` (Home tab active)
- `<StatRow />`: 4 cells (HOSTS, STACKS, APPROVALS, DEPLOYS)
- `<ActivityCard />`: left column, 3fr
- `<ActiveDeploysCard />` and `<HealthSnapshotCard />` stacked in right column, 2fr
- `<Inspector />`: rightmost rail, 340px
- `<BottomStatusBar />`: global (mounted in `app.vue`)

## States

- **Loading**: stat row shows numbers as skeleton (placeholder, not yet wired); activity feed shows the empty-state copy until events arrive
- **All-clear** (typical homelab): green stat row, "No active deploys" panel, "All systems operational" health card
- **Has approvals**: amber pending-approvals cell with count + "Review now →" inline link
- **Active deploy**: deploys card surfaces inline progress; activity feed mirrors the deploy events
- **Empty fleet** (0 hosts after wizard skip): redirect to /welcome (handled by router)

## Open questions

- Sparkline vs aggregate count for trend? StatRow currently shows absolute counts; BottomStatusBar handles the 24h trend bars. Keeping the split.
- How many activity events? 12 feels right for a desk-monitor view; mobile may want 6
- Pin-able cards? E.g., "always show prod fleet status here": defer
- Customize layout? Move panels around: out of scope, opinionated layout is the brand

## Related

- Concept: `concepts/home/v1.html` (canonical for `/`)
- Pencil divergence ADR: `decisions/2026-05-07-home-rebuild-from-v1-overrides-pencil.md`
- Implementation: `crates/isengard-plugins/dashboard/web/pages/index.vue`
- Cross: surfaces output from [[Update Policies & Approval Flow]] (approvals count) and [[Blue-Green Deployment]] (active deploys)
