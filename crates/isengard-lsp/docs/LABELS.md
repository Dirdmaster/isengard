---
title: LSP label reference
description: Machine-readable mirror of docs/concepts/labels.md. The LSP registry tests cross-reference this file.
---

# LSP label reference

This file mirrors `docs/concepts/labels.md` in machine-readable form. The
`registry_doc_sync` integration test parses every row out of this file and
asserts the LSP `REGISTRY` covers it. If you add a label to the operator
docs, mirror it here in the same shape; the test fails otherwise.

The columns are deliberately rigid:

| Column | Meaning |
|---|---|
| `Label` | Exact label key (literal or `isengard.expose.<name>.port` style pattern). |
| `Kind` | One of `Enum`, `Port`, `Url`, `Rfc3339`, `String`, `StringList`. |
| `Values` | Pipe-separated allowed values when `Kind = Enum`, free text otherwise. |

## Core

| Label | Kind | Values |
|---|---|---|
| `isengard.enable` | Enum | true |
| `isengard.stack` | String | stack name |
| `isengard.service` | String | service name |
| `isengard.cap.add` | StringList | comma-separated linux caps |
| `isengard.deploy.strategy` | Enum | recreate \| blue_green \| rolling |

## Routing (`isengard.expose.*`)

The named-rule rows (with `<name>`) cover every key whose second segment is
not in the reserved property set (`port`, `tls`, `health`, `adapter`, `auth`).

| Label | Kind | Values |
|---|---|---|
| `isengard.expose` | String | public hostname |
| `isengard.expose.port` | Port | 1..=65535 |
| `isengard.expose.tls` | Enum | acme \| edge \| manual |
| `isengard.expose.health` | String | healthcheck path |
| `isengard.expose.adapter` | Enum | none \| tailscale \| cf-tunnel |
| `isengard.expose.auth` | String | auth keyword (channel id) |
| `isengard.expose.<name>` | String | public hostname |
| `isengard.expose.<name>.port` | Port | 1..=65535 |
| `isengard.expose.<name>.tls` | Enum | acme \| edge \| manual |
| `isengard.expose.<name>.health` | String | healthcheck path |
| `isengard.expose.<name>.adapter` | Enum | none \| tailscale \| cf-tunnel |
| `isengard.expose.<name>.auth` | String | auth keyword (channel id) |

## Update policy (`isengard.policy.*`)

| Label | Kind | Values |
|---|---|---|
| `isengard.policy.strategy` | Enum | pinned \| tag-only \| minor \| any |
| `isengard.policy.gate` | Enum | auto \| approval \| never |
| `isengard.policy.paused_until` | Rfc3339 | RFC 3339 timestamp |
| `isengard.policy.on_failure` | Enum | rollback \| keep \| notify |
| `isengard.policy.approver_channel` | String | notifier channel id |

## Lifecycle hooks (`isengard.hooks.*`)

| Label | Kind | Values |
|---|---|---|
| `isengard.hooks.pre_deploy` | Url | http(s) webhook URL |
| `isengard.hooks.post_deploy` | Url | http(s) webhook URL |
| `isengard.hooks.on_failure` | Url | http(s) webhook URL |
| `isengard.hooks.secret` | String | shared HMAC secret |

## System plane (`io.isengard.*`)

Set by the platform on its own containers. The LSP recognises them so a
diagnostic can flag them as misplaced when they appear on a workload, but
their `ValueKind` still validates the value shape.

| Label | Kind | Values |
|---|---|---|
| `io.isengard.role` | Enum | controller \| agent |
| `io.isengard.api.version` | String | api contract version |
