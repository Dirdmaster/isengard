---
type: design
kind: page-spec
status: stable
created: 2026-05-02
updated: 2026-05-02
tags:
  - design
  - page
---

# Hosts

The fleet operator's home base. Lists every host across every fleet, with at-a-glance health and the path to drill into any single one.

## Audience

Self-hosters with 5-20 servers running docker-compose stacks. They arrive here either:
1. **Right after the wizard** to confirm their first host is connected
2. **From a daily check-in habit** to see if anything's wrong
3. **Triggered by an alert** to investigate a specific host

In all three cases, they want to scan quickly and either feel reassured or jump to the broken thing.

## Key interactions

- **Scan host health** — status dot color tells the whole story (green/amber/red)
- **+ Add host** (top right) — opens the wizard at step 2 with `?fresh=1`
- **Click a hostname** — opens HostInspector slide-over (right side)
- **Filter by fleet** — fleet picker chip in TopBar (`All fleets` → opens dropdown)
- **⌘K** — focus the BottomBar cmd, can `go prod-01` etc.
- **Double-click a row** — opens host detail (full page, future)

## Components used

- `<TopBar />` — chrome
- `<PageHeader title="Hosts" sub="..." cta="+ Add host" />` — title + count + CTA
- `<HostsTable :hosts="..." />` — the table itself
  - Internally uses `<StatusDot />` for the leading dot per row
- `<BottomBar />` — chrome

## States

- **Loading**: skeleton rows (5 placeholder rows with shimmering background)
- **Empty** (no hosts): use `<EmptyState />` with icon + "No hosts yet" + "Add your first server to start managing your fleet" + green "+ Add host" button
- **Populated** (typical): table with N rows
- **Error fetching**: red banner at top, table shows last-known data with stale indicator
- **Filtered** (fleet picker active): "5 hosts in prod" subtitle replaces "5 hosts across 3 fleets"

## Open questions

- ❓ Sort: by-fleet then by-name? Or just by-name? (Currently by-fleet-then-name in mocks)
- ❓ Pagination at what threshold? 50? 100?
- ❓ Should "unreachable" rows be visually demoted (greyed out) or amplified (red border)?
- ❓ Bulk actions on selected rows? (e.g. "drain 3 hosts at once") — defer to v1.x
- ❓ Inline column sorting via header click — defer to v1.x

## Related

- Concepts: `concepts/2026-05-02-hosts-v1.html`
- ADRs: `decisions/2026-05-02-bottom-bar-cmdk.md` (the BottomBar shown here)
- Implementation: `crates/isengard-plugins/dashboard/web/pages/hosts.vue` (planned)
- User flow: `flows/onboarding.md` (Hosts page is post-wizard landing)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
