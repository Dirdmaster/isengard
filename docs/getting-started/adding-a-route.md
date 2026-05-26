---
title: Adding A Route
description: Expose a service through labels or the route command.
---

# Adding A Route

Most routes should live in your compose file. Use `isd route add` for one-off containers or for routing the controller itself.

## Preferred path: one label

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

Run doctor before deploying:

```sh
isd stack doctor ./compose.yaml
```

Deploy the stack:

```sh
isd stack deploy ./compose.yaml
```

List installed routes:

```sh
isd route ls
```

## When the port is ambiguous

If a service exposes more than one likely web port, doctor asks you to choose. The explicit label is:

```yaml
labels:
  isengard.expose: gitea.isengard.app
  isengard.expose.port: "3000"
```

Use the container port, not the host-published port.

## Imperative route escape hatch

Use `isd route add` when the service is not managed by a compose file:

```sh
isd route add whoami.isengard.app --service whoami --port 80
```

Useful flags:

| Flag | Meaning |
|---|---|
| `--host <name>` | Route to a specific agent hostname. |
| `--host-id <ulid>` | Route to a specific agent id. |
| `--service <name>` | Container name or compose service name. |
| `--port <n>` | Upstream container port. |
| `--tls-mode acme` | Let Isengard terminate TLS with ACME certificates. |

Remove a route by id:

```sh
isd route rm 12
```

Verify from outside the host:

```sh
curl -I https://whoami.isengard.app
```
