---
title: stack.toml
description: Schema for the per-stack manifest.
---

# `stack.toml`

Per-stack metadata that sits beside one or more Compose files.

Minimal example:

```toml
name = "observability"
compose = ["compose.yaml"]
```

Fuller example:

```toml
name = "observability"
fleet = "lab"
compose = ["compose.yaml"]
strategy = "blue-green"
secrets = ["grafana-admin-token"]

[overlays.prod]
compose = ["compose.prod.yaml"]

[[hooks]]
on = "pre-deploy"
cmd = ["./scripts/check-config.sh"]
timeout = "30s"
on_error = "abort"
```

Fields:

| Field | Meaning |
|---|---|
| `name` | Required stack identity. |
| `fleet` | Optional fleet binding. Falls back to `isengard.toml`. |
| `compose` | Ordered Compose files, relative to the manifest directory. |
| `strategy` | `auto`, `blue-green`, `rolling`, or `recreate`. |
| `secrets` | Secret names mounted into services by the deploy path. |
| `[overlays.<name>]` | Extra Compose files selected for a deploy. |
| `[[hooks]]` | Host-side lifecycle hooks. |

Hook `on` values are `pre-deploy`, `post-deploy`, and `failure`. Hook `on_error` defaults to `abort`.
