---
type: design
kind: page-spec
status: shipped
status_note: "Phase 9e-9f shipped: queue page, ApprovalCard, nav badge, polling"
created: 2026-05-03
updated: 2026-05-06
tags:
  - design
  - page
  - approval
---

# Approvals

The pending-decision queue. Surfaces every update.pending_approval action across the fleet so an operator can act in-app rather than via Telegram/Discord callbacks.

Source design: [[Update Policies & Approval Flow]] (full architecture).

## Implementation status (2026-05-06)

- Shipped:
  - `/approvals` queue page with filter chips (Open / Decided / All)
  - `<ApprovalCard>` per row: scope path, image diff, requested-ago, expires-in countdown (warn/error tone as expiry approaches), Approve / Reject / Snooze actions, decided-state chip
  - Snooze dropdown durations: 6h / 12h / 24h / 3d / 7d (writes service-scope `paused_until` via Plan A's policy upsert)
  - `<ApprovalsBadge>` in `<TopBar>` (Approvals tab between Events and Settings)
  - `useApprovals` composable: filter, refresh, optimistic decide, 60s page poll
  - `usePendingApprovalsCount` composable: shared 30s nav-badge poller via `useState`, visibility-aware refresh
  - Empty state with in-container CTA to `/settings/policies`
- Drift from concept v1:
  - Concept renders version diffs as semver labels (`v2.4.0 → v2.4.1`); implementation renders the digest pair (`sha256:0123abcd... → sha256:fedcba98...`) because the controller stores the proposed image digest, not a parsed semver. Tag-aware rendering lands with Phase 9i (Minor strategy).
- Deferred:
  - Bulk "Approve all from staging" action (v1.x)
  - Per-card Reject reason capture (v1.x)
  - Permission gate for approver role (lands with RBAC)
  - "No notifier configured" banner: detection requires a server-side env exposure endpoint that doesn't exist yet (T5 note)

## Audience

Operator with `prod` fleet under approval gate. Telegram message arrived; they're at a desk and prefer the dashboard. Or they were away, missed the chat, and want to clear the backlog now.

## Key interactions

- **Approve / Reject** — primary actions on each card
- **Snooze 24h** — defer without rejecting (see [[Update Policies & Approval Flow]] snooze semantics)
- **Diff link** — when image is from GHCR, deep-link to GitHub compare URL
- **Filter** — kind, fleet, expiring soon
- **Recently decided** — collapsible audit table below pending list

## Components used

- `<TopBar />` (Approvals tab active)
- `<PageHeader title="Approvals" sub="N pending · earliest M ago" />`
- `<ApprovalCard />` per pending action — amber-bordered card with version diff, requester, expiry countdown, action buttons
- `<RecentlyDecidedTable />` — last 20 decisions with who/when/outcome
- `<EmptyState />` when zero pending — "No updates waiting on you. The next scan runs in 23m."
- `<BottomBar />`

## States

- **Empty** (typical idle): "No pending approvals" + next-scan countdown
- **Has pending**: cards stack newest-first, expiry pill changes color (amber → red <2h)
- **Expired** (action expired before decision): row in Recently Decided as "auto-rejected (expired)"
- **Stale** (container moved on, approval no longer relevant): card greyed with "stale" badge + auto-reject after grace
- **Bulk available** (v1.x): top action "Approve all from staging" — defer
- **No notifier configured**: banner at top "Approvals work in dashboard, but Telegram/Discord callbacks aren't set up. → Configure"

## Open questions

- ❓ Show snooze duration picker (1h / 6h / 24h / custom)? — yes, dropdown on Snooze
- ❓ Permission gate (only approver role)? — when RBAC lands; for v1, anyone with dashboard
- ❓ Audit who triggered the approval check (cron vs manual scan)? — surface in card sub
- ❓ Reject reason capture? — optional text input on Reject

## Related

- Concepts: `concepts/2026-05-02-approvals-v1.html`
- Source: [[Update Policies & Approval Flow]]
- Notifier: callbacks fire same code path as Approve/Reject buttons

---

> Shipped in Phase 9e-9f. Tracked under [[Update Policies & Approval Flow]].
