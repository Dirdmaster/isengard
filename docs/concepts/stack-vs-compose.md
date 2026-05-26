---
title: Stack Vs Compose
description: What Isengard adds around Docker Compose.
---

# Stack Vs Compose

Compose describes the containers. An Isengard stack is the operator unit wrapped around that Compose file.

## Compose owns runtime shape

Keep services, images, volumes, networks, environment, and labels in `compose.yaml`:

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

Isengard does not replace Compose syntax. It reads the same service definitions and lets Docker run them.

## Isengard owns fleet intent

The controller stores stack records, deploy history, route intent, and events. `isd stack deploy` sends the Compose file to the controller, and the selected agent applies it on a host.

Use `stack.toml` when the stack needs metadata outside Compose: a stable stack name, overlays, deploy strategy, secret names, or lifecycle hooks.

## Rule of thumb

Put container shape in Compose. Put operator intent in Isengard labels and manifests.
