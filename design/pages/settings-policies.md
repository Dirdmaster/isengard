---
type: design
kind: page-spec
status: shipped
status_note: "Phases 9a-9d, 9e-9f, 9b.1, 9c, 9d (windows) all shipped: storage, resolver, updater, REST, settings UI, preview, approval gate + Telegram + Discord callbacks, container-label discovery, maintenance windows"
created: 2026-05-03
updated: 2026-05-06
tags:
  - design
  - page
  - settings
  - policies
---

# Settings · Policies

The layered update-policy editor. Where users configure what should auto-update, what needs approval, and when updates may run.

Source design: [[Update Policies & Approval Flow]] (full schema, layering, edge cases).

## Implementation status (2026-05-06)

- Shipped:
  - Storage migration 0016 (`policies` table, polymorphic scope columns, UNIQUE constraint per scope)
  - `Policy` struct + `PolicyResolver` with provenance tracking per field
  - Updater respects `strategy=Pinned` and active `paused_until`; emits `update.policy_skipped` events
  - REST `/api/v1/policies` CRUD + `/api/v1/policies/effective` query endpoint
  - Settings → Policies page with `<PolicyRow>` list + `<PolicyEditor>` modal (field-level inheritance with override checkboxes)
  - `<EffectivePolicyPreview>` on Stack detail (per-service resolved policy with provenance labels)
  - **Phase 9e-9f**: `gate=Approval` enforcement (updater persists pending rows, surfaces them via [[approvals]] queue + Telegram interactive messages); see `design/pages/approvals.md` for the queue surface
- Shipped (cont.):
  - **Phase 9b.1**: container-scope policy rows are auto-discovered from
    `isengard.policy.*` Docker labels at agent ingest. The list view marks
    them with a "from labels" pill and renders them read-only; the editor's
    container radio is disabled with a tooltip pointing to the compose
    file. Cleanup is event-driven (on `ContainerLabelsRemoved`) plus a 1h
    reaper that drops container-scope rows whose `updated_at` is older
    than 24h.
  - **Phase 9d**: maintenance windows. `MaintenanceWindow { cron_expr, timezone }` field on Policy + ResolvedPolicy. Updater emits `update.deferred(next_window)` outside the window. PolicyEditor gains a window picker (cron + tz dropdown + custom tz + live "Next 3 firings" preview). PolicyRow renders the window summary line. EffectivePolicyPreview includes the window row with provenance. REST validates the cron expression at write time.
  - **Phase 9F**: rollback failure handler. `on_failure: Rollback` is now wired to the Phase 10 blue-green machinery. When a Rollback-policy deployment fails healthcheck or post-switch collapses, the supervisor re-pulls the captured `previous_digest` and recreates the container at that pinned image. New deployment states `rolling_back`, `rolled_back`, `rollback_failed`. New event kinds `update.rolled_back`, `update.rollback_failed`. DeploymentInProgressPanel + DeploymentAbortedPanel render the new states with appropriate badges + Retry behaviour. `Keep` adds a 24h `paused_until` upsert on the service-scope policy row.
- Deferred:
  - `Minor` strategy semver-aware bumping (Phase 9i)
  - Discord interactive messages (Phase 9g; same pattern as Telegram)

## Route

`/settings/policies`

## Layout

A vertical stack of policy rows, ordered most-general → most-specific:

1. **GLOBAL DEFAULT** (always present, always topmost)
2. **FLEET · <name>** rows (one per fleet that has overrides)
3. **STACK · <fleet> / <stack>** rows
4. **SERVICE · <fleet> / <stack> / <service>** rows
5. **CONTAINER** rows (only if explicit container-label overrides exist)

Each row shows: scope label · gate badge · the fields it OVERRIDES (not the resolved policy) · Edit / Remove / Resume buttons.

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Update policies" cta="+ Add policy" />`
- `<SettingsTabs active="policies" />`
- `<PolicyRow />` — bordered card per row, with hierarchy indent
- `<PolicyEditor />` (modal) — fields-with-inheritance form: each field shows inherited value as placeholder, "override" checkbox activates input
- `<EffectivePolicyPreview />` (collapsible side panel) — pick a container, see the resolved policy with provenance per field
- `<BottomBar />`

## States

- **First-time** (only GLOBAL DEFAULT present): single row, "+ Add fleet/stack policy" CTA prominent, link to docs
- **Stable**: rows stack vertically with subtle hierarchy indent
- **Pending sync** (policy edited, not yet pushed to agents): yellow indicator on row + "syncing…" pill
- **Conflict** (somehow two rows for same scope — shouldn't happen with UNIQUE constraint but defensive): red banner asking to merge
- **Effective preview open**: side panel slides in from right; container picker at top, resolved policy table below

## Editor field semantics

Each field is one of: `strategy`, `gate`, `window`, `paused_until`, `on_failure`, `approver_channel`. UI per field:
- Inherited value shown as placeholder + provenance label ("inherited from FLEET · prod")
- Checkbox "Override at this level" enables the input
- Clearing the override returns to inherited

The `window` field uses standard 5-field cron syntax + an IANA timezone dropdown (UTC, Europe/Zurich, America/New_York, Asia/Tokyo, custom). When overridden, the editor renders a "Next 3 firings" preview computed client-side via a small bounded walker. Outside the window, the updater emits `update.deferred(next_window)` and skips recreate. Default window duration after a firing: 1h.

## Open questions

- ❓ Drag-to-reorder within same scope? — not meaningful (precedence is by scope kind)
- ❓ Bulk apply (e.g. "set all stacks in prod to approval")? — UX is repetitive, defer to v1.x
- ❓ Diff view "what would change if I save this?" — yes, mini banner above Save

## Related

- Concepts: `concepts/2026-05-03-settings-policies-v1.html`
- Source: [[Update Policies & Approval Flow]]
- Cross: Stack detail Settings tab embeds `<EffectivePolicyPreview />` scoped to that stack

---

> Approvals tab now lives in TopBar (shipped Phase 9e-9f). See [[approvals]] for the queue surface.
