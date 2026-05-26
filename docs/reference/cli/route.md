---
title: isd route
description: Reference for the `isd route` subcommand.
---

# `isd route`

Manage public routing rules stored by the controller.

Prefer labels for managed stacks:

```yaml
labels:
  isengard.expose: whoami.isengard.app
```

Use `isd route` for one-off containers, controller routes, and debugging.

## List routes

```sh
isd route ls
```

Columns:

| Column | Meaning |
|---|---|
| `HOSTNAME` | Public hostname matched by Pingora. |
| `UPSTREAM` | Container name or compose service name. |
| `PORT` | Upstream container port. |
| `TLS` | TLS mode: `acme`, `edge`, or `manual`. |
| `SRC` | Origin tag, usually `ui` or `compose`. |

## Add a route

```sh
isd route add whoami.isengard.app --service whoami --port 80
```

Omit fields to use the interactive picker:

```sh
isd route add
```

Flags:

| Flag | Meaning |
|---|---|
| `--host <name>` | Resolve an agent by hostname. |
| `--host-id <ulid>` | Use an agent id directly. |
| `--service <name>` | Container name or compose service name. |
| `--port <n>` | Upstream container port. |
| `--protocol http` | Upstream protocol, `http` or `https`. |
| `--adapter none` | Networking adapter: `none`, `tailscale`, or `cf-tunnel`. |
| `--tls-mode acme` | TLS mode: `acme`, `edge`, or `manual`. |
| `--healthcheck-path /healthz` | Optional upstream healthcheck path. |

## Remove a route

```sh
isd route rm 12
```

Find the id with `isd route ls`.

## Troubleshooting

Run doctor on the source compose file first:

```sh
isd stack doctor ./compose.yaml
```

If a hostname returns `404`, no route exists for that host. If it returns `503`, a route exists but the upstream is unresolved or unhealthy.
