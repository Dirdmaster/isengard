---
type: design
kind: page-spec
status: draft
status_note: "Sub-tabs (Adapter/Proxy) shipped; per-adapter model + listener config + ACME log + import deferred"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
  - settings
  - networking
---

# Settings · Networking

## Implementation status (2026-05-05)

- Shipped: Adapter cards (None / Tailscale / CfTunnel), `RoutingRulesTable`, `RoutingRuleEditModal`
- Deferred: Headscale / raw-wireguard / custom adapter cards, true `<NetworkingSubTabs />` separation (Adapter and Proxy currently render stacked on one tab), Listener config (ports, default TLS strategy), ACME contact email + issuance log, `<ImportRulesModal />` (NPM / Traefik), label-vs-UI conflict banner
- Drift: cards are scoped per-host (assume `hostsStore.hosts[0]`) instead of per-adapter; multi-adapter selection per routing rule is not represented


The control plane for Pingora + NetworkingAdapter configuration. Two sub-tabs:
1. **Adapter** — which transport(s) the fleet uses
2. **Proxy** — Pingora settings + routing rules table

Source design: [[Networking & Proxy]].

## Route

`/settings/networking` (default sub-tab: Adapter; query `?tab=proxy` for Proxy)

## Sub-tab: Adapter

Cards per adapter (cf-tunnel, tailscale, headscale, raw-wireguard, custom). Each shows:
- Status (configured ✓ / not configured / failed)
- Adapter-specific config block (collapsed when configured, expanded when adding)
- Test button
- Per-adapter health (e.g. cf-tunnel: 4 colos online; tailscale: 3 nodes in tailnet)

Multi-adapter is allowed — routing rules pick which adapter to use per rule.

## Sub-tab: Proxy

- **Listener config** — ports (default 8443 HTTPS, 8080 HTTP), default TLS strategy
- **ACME** — contact email, issuance log (last 5 issues with status)
- **Routing rules table** — the unified UI/label/imported list per [[Networking & Proxy]] hybrid model
  - Columns: HOSTNAME / TARGET / ADAPTER / TLS / HEALTH / SOURCE
  - Row click → inline editor for UI rules; modal explainer for label/imported rules
  - + Add routing rule (top right) → modal
  - Import button → NPM JSON / Traefik dynamic.yml paste

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Networking · <sub-tab>" />`
- `<SettingsTabs active="networking" />`
- `<NetworkingSubTabs />` — Adapter / Proxy
- `<AdapterCard />` per adapter
- `<RoutingRulesTable />` (Proxy tab)
- `<RoutingRuleEditor />` (modal) — hostname, target service, port, TLS, health path, adapter
- `<ImportRulesModal />` — NPM/Traefik bulk import
- `<BottomBar />`

## States

- **No adapter configured**: prominent "Choose an adapter to start exposing services" hero card
- **Adapter healthy**: green status, condensed config view
- **Adapter failed**: red banner with last error + Retry
- **Routing rules empty**: empty state explaining label-vs-UI workflow with "Add your first rule" + "Or learn about labels" links
- **Routing rule conflict** (label arrived for existing UI hostname): inline banner per [[Networking & Proxy]] conflict resolution
- **ACME issuance in progress**: yellow row in issuance log
- **TLS cert expiring < 14 days**: amber badge on rule row

## Open questions

- ❓ Adapter ordering when multiple configured (UI default vs explicit)? — settings dropdown, default = first configured
- ❓ Per-rule TLS toggle persists when rule source = label? — yes, via routing_rule_overrides per [[Networking & Proxy]]
- ❓ Inline cert upload (manual mode) — file input or paste textarea? — paste

## Related

- Concepts: `concepts/2026-05-02-settings-networking-v1.html`
- Source: [[Networking & Proxy]], [[Cloudflare Integration]]

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
