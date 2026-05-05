---
type: design
kind: page-spec
status: stable
created: 2026-05-03
updated: 2026-05-03
tags:
  - design
  - page
  - settings
  - policies
---

# Settings · Policies

The layered update-policy editor. Where users configure what should auto-update, what needs approval, and when updates may run.

Source design: [[Update Policies & Approval Flow]] (full schema, layering, edge cases).

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

## Open questions

- ❓ Drag-to-reorder within same scope? — not meaningful (precedence is by scope kind)
- ❓ Bulk apply (e.g. "set all stacks in prod to approval")? — UX is repetitive, defer to v1.x
- ❓ Diff view "what would change if I save this?" — yes, mini banner above Save

## Related

- Concepts: `concepts/2026-05-03-settings-policies-v1.html`
- Source: [[Update Policies & Approval Flow]]
- Cross: Stack detail Settings tab embeds `<EffectivePolicyPreview />` scoped to that stack

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
