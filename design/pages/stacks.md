---
type: design
kind: page-spec
status: draft
status_note: "+ Add stack CTA missing (needs Add Stack wizard); IMAGE/STATE/LAST DEPLOY columns + StatusDot dropped"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
---

# Stacks

## Implementation status (2026-05-05)

- Shipped: stacks list with Stack/Host/Fleet/Services/Latest event/Source columns, fleet filter via TopBar
- Deferred: `+ Add stack` CTA (depends on Add Stack wizard), per-row `<StatusDot />`, `IMAGE` / `STATE` / `LAST DEPLOY` columns, source-icon badges (📦 paste · 🌱 git · 🎨 form · 🔍 discovered), failed-deploy red border state
- Drift: subtitle reads `"N stacks"` instead of `"N stacks · M services"` and drops the `"in <fleet>"` filter scope


The list of every stack across the fleet. A stack is a docker-compose project (logical group of services that ship together).

## Audience

Operators looking for a specific stack to deploy/restart/inspect. Also the entry point for "I want to add something new to my fleet."

## Key interactions

- **Click a row** — opens stack detail (`/stacks/[id]`)
- **+ Add stack** (top right) — opens stack wizard with mode picker (paste compose / git-sync / form builder; see [[Stack Deployment]])
- **Filter by fleet** — TopBar fleet picker filters table
- **Source column** — badges show origin (📦 paste · 🌱 git · 🎨 form · 🔍 discovered) per [[Stack Deployment]] source-tracking
- **⌘K** — `deploy <stack>` jumps cursor here

## Components used

- `<TopBar />` (Stacks tab active)
- `<PageHeader title="Stacks" sub="N stacks · M services" cta="+ Add stack" />`
- `<StacksTable :stacks />`
  - Columns: HOSTNAME / IMAGE / SERVICES / STATE / LAST DEPLOY / SOURCE
  - StatusDot leading
- `<BottomBar />`

## States

- **Loading**: 5 skeleton rows
- **Empty** (0 stacks): EmptyState with "+ Add your first stack" + brief explainer of compose-as-source-of-truth
- **Populated**: table
- **Filtered**: subtitle reflects filter scope ("3 stacks in prod")
- **Failed deploy**: stack row shows red border + "deploy failed 12m ago" overlay; click to see attempts log

## Open questions

- ❓ Group by fleet? Or flat list with FLEET column? — flat list, filter by fleet via TopBar
- ❓ Inline expand to show services without leaving the page? — defer; service detail is the home for deep drill
- ❓ Sort by recency-of-deploy vs alpha? — alpha by default, sort menu in v1.x

## Related

- Concepts: `concepts/2026-05-02-stacks-v1.html`
- Stack detail: `pages/stack-detail.md`
- Source design: [[Stack Deployment]]

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
