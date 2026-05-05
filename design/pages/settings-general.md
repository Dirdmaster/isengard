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
---

# Settings · General

Catch-all for instance-level settings that don't fit a specialized tab.

## Route

`/settings/general`

## Sections

- **Controller identity** — public URL (used for docker-run install commands), instance name, timezone (used by maintenance windows)
- **Telemetry** — opt-in anonymous usage stats (default OFF), crash reports (default OFF)
- **Updates** — check for new isengard releases (default weekly), update channel (stable / beta)
- **Defaults** — default fleet for new hosts (NULL = require explicit), default update gate (auto / approval)
- **Danger zone** — Reset welcome wizard · Wipe inventory (with double-confirm)

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="General" />`
- `<SettingsTabs active="general" />` — General¹ · Enrollment¹ · Networking¹ · Deployments¹ · Notifier¹ · Policies² · Backup² · Webhooks² · Authentication²

  ¹ Shipped. ² Designed-not-built — the page-spec exists in `design/pages/` as a placeholder for an unshipped phase, and the tab is not rendered by the live `SettingsTabs` component yet. Compare this list against `crates/isengard-plugins/dashboard/web/pages/settings/index.vue` for the source of truth on what the dashboard actually renders.
- `<SettingsSection />` per group with rows of label / control / helper text
- `<DangerZone />` red-bordered section at bottom
- `<BottomBar />`

## Open questions

- ❓ Locale beyond timezone? — defer
- ❓ Light theme? — defer; dark only for v1

## Related

- Concepts: (none yet — straightforward form layout)
- Settings tabs: see other `pages/settings-*.md`
