---
type: design
kind: page-spec
status: stable
created: 2026-05-05
updated: 2026-05-06
status_note: "Phase 10c added per-stack parallelism dropdown to the stack group header (1 / 2 / 3 / all)"
tags:
  - design
  - page
  - settings
  - deployments
---

# Settings · Deployments

Per-service override of how new images are rolled out. `Auto` (default) consults container labels first, then falls back to the controller default (blue-green for HTTP-routed services, in-place for everything else). `Blue-green` and `In-place` force one or the other regardless of detection.

Source design: `docs/superpowers/specs/2026-05-04-phase-10e-10g-blue-green-ui-design.md` (Phase 10g: Settings tab) · `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md` (Phase 10a:d: driver).

## Route

`/settings/deployments`

## Sections

- **Deploy strategy**: single section with one stack-grouped table
  - Group header: stack name (uppercased, faint, mono); services without a stack bucket under `(no stack)`
  - Group header right-side: **Multi-host parallelism dropdown** (Phase 10c): `Rolling (1)` / `Parallel 2` / `Parallel 3` / `All at once`. Persists to `POST /api/v1/stacks/:id/deployment-parallelism`. Hidden for the `(no stack)` bucket. Tooltip explains: "How many hosts deploy in lockstep when this stack runs on multiple hosts. Defaults to rolling (1 at a time)."
  - Per-row columns: SERVICE (mono, primary text) · STRATEGY (3-button toggle: Auto · Blue-green · In-place)
  - Active button is highlighted with the info accent + 10% bg fill; inactive buttons stay subtle
  - Click a button to flip the override; PUT fires immediately (no Save button) and a toast confirms (`Set <service> to blue-green` / `Cleared override`)
- **Auto explainer**: section description: "Auto picks blue-green for HTTP-routed services and in-place for everything else." Container-label override (`isengard.deploy.strategy=...`) still wins over UI selection: that precedence is documented in the phase spec but not visualized in v1.

## Components used

- `<AppShell />` + `<PageHeader title="Settings" subtitle="Controller configuration" />`
- `<SettingsTabs active="deployments" />`
- `<DeploymentsSettings />`: top-level container
- `<SettingsSection title="Deploy strategy" />`
- Inline 3-button toggle (no shared component yet: plain `<button>` triplet with `aria-pressed`)
- `useToast()` for success / failure feedback

## States

- **Loading** (initial mount, no cached data): "Loading services..." text
- **Empty fleet** (no services discovered): "No services discovered yet. Once an agent reports its containers, they appear here."
- **All Auto** (fresh install): every row shows Auto highlighted; no override rows in the DB
- **Mixed** (some overrides set): override rows show Blue-green or In-place highlighted; rest stay on Auto
- **Single-stack scope**: when only one stack exists, the page collapses visually to one group: no special UI, just shorter
- **Save in flight**: button click is optimistic-ish: the PUT runs and the list refreshes on success; no spinner per row in v1
- **Save failed**: toast `Save failed: <error>`; row reverts to its previous value on next refresh
- **List load failed**: red error string under the section header

## Open questions

- ❓ Per-stack default override (apply to all services in this stack)?: defer; v1 is per-service only. Stack-level default would need a new column on `stacks` and precedence rules vs per-service.
- ❓ Per-fleet / controller-wide default override?: defer. The current "default" is hardcoded (HTTP-routed → blue-green, else in-place). Surfacing it as a settable knob is a v1.x ask.
- ❓ Bulk apply (multi-select rows → "Set to in-place")?: defer; keep v1 single-row clicks. Bulk patterns can wait until operators ask.
- ❓ Per-strategy validation (warn when forcing blue-green on a service without a healthcheck)?: deferred per Phase 10g spec ("Eligibility annotations" row in deferred-features table). Eligibility data isn't on the `services` table; user sees the actual classification surface in the Events feed when the next deployment fires.
- ❓ Show container-label override inline ("Container label: blue-green (overrides settings)")?: Phase 10g design called for this; not in v1 ship. Revisit when an operator gets confused by a UI choice that didn't take effect.
- ❓ Disable buttons for services that don't support a strategy (e.g. blue-green requires healthcheck)?: no; eligibility detection is per-deployment, not per-service. Showing every option keeps the UI honest about what the override does (force the strategy; the driver handles incompatibility).
- ❓ Deployment history / abort UI on this page?: out of scope. Active deployments live on `pages/stack-detail.md` (`<DeploymentInProgressPanel />`, `<DeploymentAbortedPanel />`); history is Plan C 10h, not yet shipped.

## Related

- Phase spec (UI): `docs/superpowers/specs/2026-05-04-phase-10e-10g-blue-green-ui-design.md`
- Phase spec (driver): `docs/superpowers/specs/2026-05-04-phase-10a-10d-blue-green-core-design.md`
- Backend endpoints: `crates/isengard-plugins/dashboard/src/deployments.rs`: `GET /services/deploy-strategy`, `PUT /services/:id/deploy-strategy`
- Active deployment surfaces (out of scope here): `pages/stack-detail.md` for the in-progress / aborted panels
- Container label override (always wins): `isengard.deploy.strategy=blue-green|in-place` documented in Phase 10a:d driver spec

---

> Approvals tab is pending Phase 9: not currently in TopBar.
