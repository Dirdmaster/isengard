---
type: design
kind: page-spec
status: draft
status_note: "Tab structure shipped (Overview/History real, others stubs); compose/policies/promote actions deferred"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
---

# Stack detail

## Implementation status (2026-05-05)

- Shipped: stack page with services as `<ServiceChip>` row, `Force update stack` action, `DeploymentInProgressPanel` / `DeploymentAbortedPanel` (Phase 10), Recent events panel, `<EffectivePolicyPreview>` per service (Phase 9d)
- Deferred: tab structure (Overview / Compose / History / Routing / Settings), Redeploy / Abort / Promote / Pause action cluster, `<ComposePane />`, `<HistoryTimeline />` (deploy attempts, not raw events), per-stack `<RoutingRulesTable />`, `<EffectivePolicyCard />`, header status chip
- Drift: header uses a `← Stacks` link instead of a breadcrumb; services are inline chips, not a clickable `<ServicesTable />` (because service-detail isn't built)


Per-stack page. The drill-down from /stacks. Surfaces everything about one compose project: services, deploy state, routing, history.

## Audience

Operator who picked a stack to inspect or act on. They want service status at a glance, ability to redeploy / abort / promote, and a clear path to per-service detail when something is wrong.

## Key interactions

- **Breadcrumb** — `Stacks / <stack>` links back
- **Header status chip** — running / deploying / aborted / paused
- **Actions** — Redeploy / Abort deploy / Promote (blue-green) / Pause updates
- **Services table** — click row → service detail
- **Tabs** — Overview / Compose / History / Routing / Settings (per-stack policies)
- **Deploy progress card** — surfaces when state=deploying; see [[Blue-Green Deployment]]

## Components used

- `<TopBar />`
- `<PageHeader />` with breadcrumb, title, status chip, action cluster
- `<DeployProgressCard />` — only when deploying or aborted
- `<Tabs />` — Overview / Compose / History / Routing / Settings
- `<ServicesTable />` — Overview tab default body
- `<ComposePane />` — Compose tab (source view + edit mode link)
- `<HistoryTimeline />` — History tab (deploy attempts, with status + duration)
- `<RoutingRulesTable :scope="stack" />` — Routing tab (rules attached to this stack)
- `<EffectivePolicyCard />` — Settings tab (resolved policy with provenance per [[Update Policies & Approval Flow]])
- `<BottomBar />`

## States (key variants)

- **running** (typical): green chip, services all green, "Last deploy: 2 days ago"
- **deploying**: amber chip, DeployProgressCard pinned at top with progress bar + per-host phase, Abort button visible
- **aborted**: red chip, last 3 attempts log expanded, Retry / Adjust buttons visible (see [[Blue-Green Deployment]])
- **paused**: muted chip, "Updates paused until <date>" banner, Resume button
- **healthy with degraded service**: green stack chip, but services table shows one amber + Investigate link to service detail
- **deleted source** (compose paste deleted, stack still running): warning banner "compose source missing — running from cached spec"

## Open questions

- ❓ Inline edit compose vs link to external editor? — inline read-only with "edit in editor" CTA, defer in-place editing
- ❓ Per-service Promote button (per-service blue-green) vs whole-stack? — whole-stack default, per-service in advanced
- ❓ Audit log (who triggered what) on this page or only on Events? — link to filtered Events view; don't duplicate

## Related

- Concepts: `concepts/2026-05-02-stack-detail-deploying-v1.html`, `concepts/2026-05-02-stack-detail-aborted-v1.html`
- Service drill: `pages/service-detail.md`
- Source: [[Blue-Green Deployment]], [[Stack Deployment]]

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
