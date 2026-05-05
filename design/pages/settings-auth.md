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
  - auth
---

# Settings · Authentication

Configure dashboard auth: none (single-user homelab default) · shared-token (LAN multi-user) · Cloudflare Access (delegated SSO).

Source design: [[Cloudflare Integration]] (cf-access section).

## Route

`/settings/auth`

## Sections

1. **Mode selector** — radio: None · Shared token · Cloudflare Access
2. **Mode-specific config**:
   - **None**: explainer "Anyone with network access can use the dashboard. Recommended only for trusted LANs."
   - **Shared token**: token value (rotate button), expiry, allowed-IPs allowlist
   - **Cloudflare Access**: team domain, audience tag, allowed emails as removable chips, JWKS cache status with key count + last refresh
3. **Sessions** — active dashboard sessions table (cookie-based; rotate token to invalidate all)

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Authentication" />`
- `<SettingsTabs active="auth" />`
- `<AuthModeSelector />` — radio cards
- `<SharedTokenCard />`
- `<CloudflareAccessCard />`
- `<SessionsTable />`
- `<BottomBar />`

## States

- **None mode**: yellow banner "Dashboard is unauthenticated"
- **Shared token mode**: green status, token visible after click-to-reveal
- **Cloudflare Access mode**: green status if JWKS recently refreshed; amber if stale; red if unreachable
- **Switching modes**: confirmation modal "All current sessions will be logged out. Continue?"

## Open questions

- ❓ Per-user RBAC (approver / viewer / admin)? — defer to v1.x; cf-access handles user identity, RBAC is internal
- ❓ Audit log for login attempts? — yes, surface in Events feed (kind=`auth.login`)
- ❓ Magic-link login as fourth mode? — defer; probably v2 SaaS only

## Related

- Source: [[Cloudflare Integration]]
- Concepts: (none yet)

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
