---
title: Expose a service on Tailscale
description: Route a service through the Tailscale adapter.
---

# Expose a service on Tailscale

Use the Tailscale adapter when a service should be reachable through tailnet routing instead of a direct public listener.

Example service:

```yaml
services:
  grafana:
    image: grafana/grafana:11.5.2
    labels:
      isengard.expose: grafana.isengard.app
      isengard.expose.port: "3000"
      isengard.expose.adapter: tailscale
      isengard.expose.tls: edge
```

Check the file before deploying:

```sh
isd stack doctor ./compose.yaml
```

Deploy and confirm the route:

```sh
isd stack deploy ./compose.yaml
isd route ls
```

Use `isengard.expose.adapter: none` for the direct Pingora path, and `tailscale` when the route should be mediated by Tailscale.
