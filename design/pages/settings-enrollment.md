---
type: design
kind: page-spec
status: stable
created: 2026-05-05
updated: 2026-05-05
tags:
  - design
  - page
  - settings
  - enrollment
---

# Settings · Enrollment

Mint single-use enrollment tokens for new agents and revoke unredeemed ones. The complementary per-host cert revoke lives on `HostInspector`, not here (cross-referenced below).

Source design: [[2026-05-05 Auth and Identity Swarm Style]] (decision) · `docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md` (Phase 14 spec).

## Route

`/settings/enrollment`

## Sections

- **Agent enrollment** — entry points to add a new host
  - `+ Add host with wizard` (primary): re-runs the welcome wizard from step 1 with `?fresh=1`
  - `Generate install command (advanced)`: opens `<AddHostModal />` for operators who already know the drill
- **Active enrollment tokens** — table + Mint button
  - Header row: `<count> active` (or "Loading…" / error message) on the left, `Mint token` button on the right
  - Table columns: HASH PREFIX (16 hex chars, mono) · ROLE (currently always `agent`) · CREATED · EXPIRES · ACTIONS (Revoke)
  - Empty state: dashed-border block "No active enrollment tokens. Mint one to enroll a new agent."
- **Mint token modal** (`<MintTokenModal />`) — two-step flow
  1. Pre-mint: TTL input (minutes, 1–1440, default 15) with validation
  2. Post-mint: plaintext token (shown exactly once) + ready-to-paste `docker run` snippet with `ISENGARD_CONTROLLER` + `ISENGARD_ENROLL_TOKEN` env vars; per-block "Copy" buttons; expiry timestamp under the token
- **Per-host cert revoke** — NOT on this page. See `pages/hosts.md` → `<HostInspector />` "Revoke cert" action. Surfaced here only as a cross-reference because the underlying composable (`useEnrollment.revokeHostCert`) is shared.

## Components used

- `<AppShell />` + `<PageHeader title="Settings" subtitle="Controller configuration" />`
- `<SettingsTabs active="enrollment" />`
- `<EnrollmentSettings />` — top-level container
- `<SettingsSection title="Agent enrollment" />` + `<SettingsSection title="Active enrollment tokens" />`
- `<AddHostModal />` (advanced install-command generator)
- `<MintTokenModal />` (TTL form → minted-token result with `docker run` snippet)
- `<ConfirmDialog />` via `useConfirm()` for the revoke confirmation

## States

- **Empty** (no active tokens): dashed-border placeholder with inline "Mint one" link
- **Just minted** (modal still open): token visible in plaintext + copy buttons + amber "shown once" emphasis
- **One or more active**: rows sorted by `created_at desc`; each row revokable independently
- **Revoke in flight**: confirmation modal with `Revoke token <hash_prefix>?` + danger-styled confirm
- **Revoked / cancelled**: row removed on next `refresh()` (cancelled tokens are hidden — see open question)
- **Expired**: row removed automatically when the controller's `list_active_tokens` filter excludes it; no UI for "recently expired"
- **List load failed**: red error string in the header row instead of count
- **Mint failed**: inline red error inside the modal + toast

## Open questions

- ❓ Should cancelled / expired tokens be visible (audit history)? — currently no; backend has the data (`enrollment_token.cancelled_at` from migration 0015) but the dashboard filters them out. Defer until Events feed surfaces `enrollment.token.minted` / `.revoked` / `.expired` consistently.
- ❓ TTL preset chips (5m / 15m / 1h / 24h) vs free-form number? — current UI is free-form. Presets would be a one-line addition; revisit if operators report fat-fingering.
- ❓ Show 16-char hash prefix vs full hash vs redacted token? — current UI shows the 8-byte (16 hex char) prefix. Long enough to be unique within a fleet, short enough to glance-compare. Don't show the plaintext token after mint — that's the whole security model.
- ❓ Token rotation flow (mint replacement + old still valid)? — out of scope. Mint a new one, revoke the old one. Two-step intentional.
- ❓ Multiple roles (admin / operator / agent)? — deferred per [[2026-05-05 Auth and Identity Swarm Style]]. Backend type is `TokenRole` enum but only `Agent` is wired.
- ❓ Audit log of mint / revoke events on this page? — defer; lives in the global Events feed (`kind=enrollment.*`).

## Related

- Decision: [[2026-05-05 Auth and Identity Swarm Style]]
- Phase spec: `docs/superpowers/specs/2026-05-05-phase-14-auth-and-identity-design.md`
- Backend endpoints: `crates/isengard-plugins/dashboard/src/enrollment.rs` — `POST /enrollment/tokens`, `GET /enrollment/tokens`, `DELETE /enrollment/tokens/:hash_prefix`, `DELETE /hosts/:host_id/cert`
- Cross-reference: `pages/hosts.md` (HostInspector hosts the per-host cert revoke action)
- Welcome wizard: `pages/welcome.md` (the `+ Add host with wizard` entry point)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
