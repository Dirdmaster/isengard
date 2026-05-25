---
title: Labels
description: The isengard.* label vocabulary on a service or container.
---

# Container labels

Operator-facing vocabulary on `labels:` of a service or container. Traefik-style: declarative metadata the agent reports up to the controller, which decides what to route, when to deploy, where to push events.

All keys are dot-namespaced under `isengard.*`. System-plane labels (controller / agent's own containers) live under `io.isengard.*` so they can't collide with user labels.

## Quick reference

| Label | What it does | Values | Source |
|---|---|---|---|
| `isengard.enable` | Opt the container into the updater + policy pipeline | `true` (else: ignored) | `crates/isengard-plugins/updater/src/labels.rs` |
| `isengard.stack` | Override the stack name (defaults to compose project) | string | `crates/isengard-agent/src/compose_apply.rs`, `container_snapshot.rs` |
| `isengard.service` | Override the service name (defaults to container name) | string | `crates/isengard-agent/src/compose_reconciler.rs` |
| `isengard.cap.add` | Mirror of compose `cap_add:` for protection-layer checks | comma-separated caps | `crates/isengard-agent/src/compose_apply.rs` |
| `isengard.deploy.strategy` | Per-service deploy strategy override | `recreate` / `blue_green` / `rolling` | `crates/isengard-plugins/updater/src/dispatch_helpers.rs` |

## Routing (`isengard.expose.*`)

Drive `Pingora` routing rules. Multiple named rules supported per container.

| Label | What it does | Values |
|---|---|---|
| `isengard.expose` | Default rule: public hostname | e.g. `whoami.isengard.app` |
| `isengard.expose.port` | Default rule: upstream port | `1..=65535` |
| `isengard.expose.tls` | Default rule: TLS termination mode | `acme` / `edge` / `manual` |
| `isengard.expose.health` | Default rule: healthcheck path | e.g. `/healthz` |
| `isengard.expose.adapter` | Default rule: networking adapter | `none` / `tailscale` / `cf-tunnel` |
| `isengard.expose.auth` | Default rule: auth requirement | free-form (notifier channel id) |
| `isengard.expose.<name>` | Named rule N: public hostname (e.g. `.web`, `.api`) | hostname |
| `isengard.expose.<name>.port` | Named rule N: upstream port | `1..=65535` |
| `isengard.expose.<name>.tls` | Named rule N: TLS mode | `acme` / `edge` / `manual` |
| `isengard.expose.<name>.health` | Named rule N: healthcheck path | path |
| `isengard.expose.<name>.adapter` | Named rule N: adapter | `none` / `tailscale` / `cf-tunnel` |
| `isengard.expose.<name>.auth` | Named rule N: auth | string |

`<name>` is anything that isn't a known property keyword (`port`, `tls`, `health`, `adapter`, `auth`). Common patterns: `.web`, `.api`, `.admin`.

Example:
```yaml
labels:
  isengard.expose.web: grafana.isengard.app
  isengard.expose.web.port: "3000"
  isengard.expose.admin: admin.isengard.app
  isengard.expose.admin.port: "9090"
  isengard.expose.admin.auth: oncall
```

Parser: `crates/isengard-core/src/labels.rs::parse_labels`.

## Update Policy (`isengard.policy.*`)

Tell the updater how to handle this container.

| Label | Field | Values |
|---|---|---|
| `isengard.policy.strategy` | Image-version match | `pinned` / `tag-only` / `minor` / `any` |
| `isengard.policy.gate` | Approval gate | `auto` / `approval` / `never` |
| `isengard.policy.paused_until` | Pause updates until | RFC 3339 timestamp |
| `isengard.policy.on_failure` | What to do on failure | `rollback` / `keep` / `notify` |
| `isengard.policy.approver_channel` | Notifier channel to ping | string (channel id) |

Parser: `crates/isengard-core/src/policy/labels.rs::parse_policy_labels`. Unknown `isengard.policy.<future>` keys are ignored for forward compatibility.

## Lifecycle Hooks (`isengard.hooks.*`)

Webhook URLs fired during the deploy lifecycle. All optional.

| Label | What it does |
|---|---|
| `isengard.hooks.pre_deploy` | Webhook URL fired before recreate |
| `isengard.hooks.post_deploy` | Webhook URL fired after healthy |
| `isengard.hooks.on_failure` | Webhook URL fired on deploy failure |
| `isengard.hooks.secret` | Shared secret for webhook HMAC signing |

Parser: `crates/isengard-core/src/hooks.rs::parse_hook_labels`.

## SSH Bastion (`isengard.ssh.*`)

Per-host knobs for the SSH bastion. The LSP recognises the vocabulary today; runtime enforcement (controller + agent actually reading these at mint and install time) lands in a later phase.

| Label | What it does | Values |
|---|---|---|
| `isengard.ssh.disabled` | Skip SSH CA install on this host | `true` / `false` |
| `isengard.ssh.principals` | Override default cert principals (default: `isengard`) | comma-separated list |
| `isengard.ssh.max_ttl_seconds` | Per-host cap on minted SSH cert TTL, in seconds | u32 (capped at the controller's 24h ceiling) |
| `isengard.ssh.allowed_users` | Whitelist of SSH usernames allowed to dial this host | comma-separated list (empty = allow all) |

### `isengard.ssh.disabled`

Set to `true` to skip installing the SSH CA on this host's sshd. Same effect as the `ISENGARD_SSH_BASTION_DISABLED=1` environment variable, but per-host and label-driven. Useful for taking one host out of the bastion without redeploying the agent.

```yaml
labels:
  isengard.ssh.disabled: "true"
```

### `isengard.ssh.principals`

Comma-separated list of SSH principals embedded in certs minted for this host. Defaults to `isengard`. Use this when the host's `authorized_principals` file lists a non-default identity.

```yaml
labels:
  isengard.ssh.principals: "admin,deploy"
```

### `isengard.ssh.max_ttl_seconds`

Per-host override of the controller's `ISENGARD_SSH_CERT_MAX_TTL` ceiling, in seconds. The controller's absolute 24h ceiling (86400 seconds) still applies; this label can only tighten the cap, never raise it.

```yaml
labels:
  isengard.ssh.max_ttl_seconds: "3600"
```

### `isengard.ssh.allowed_users`

Comma-separated whitelist of SSH usernames. When set, the controller rejects mint requests whose principal is not in this list. Empty / unset means allow all.

```yaml
labels:
  isengard.ssh.allowed_users: "isengard,deploy"
```

## System Plane (`io.isengard.*`)

Set by the platform on its own containers. Don't put these on workload containers.

| Label | What it does | Values |
|---|---|---|
| `io.isengard.role` | Identifies controller / agent containers for discovery | `controller` / `agent` |
| `io.isengard.api.version` | API contract version of the controller image | `"1"` (current) |

Defined in `crates/isd-runtime/src/discovery_labels.rs`.
