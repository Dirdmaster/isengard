---
title: isd upgrade
description: Reference for the `isd upgrade` subcommand.
---

# `isd upgrade`

Pull a newer controller and agent image tag, recreate the system containers, and wait for the controller to become ready.

```sh
isd upgrade
```

The default target is the current tag, repulled to refresh a moving image such as `next`. Pin an explicit tag when upgrading to a release:

```sh
isd upgrade --tag v0.7.0
```

By default `upgrade` takes an encrypted backup first. If the post-upgrade health check fails, the error includes the backup path so you can restore.

Useful flags:

| Flag | Meaning |
|---|---|
| `--tag <tag>` | Upgrade to a specific image tag. |
| `--skip-backup` | Recreate without a pre-upgrade backup. Use only when state loss is acceptable. |
| `--yes` | Skip the confirmation prompt. |
| `--wait-secs <n>` | Override the readiness wait window. Default: `240`. |
| `--no-wait` | Return after recreating containers and watch logs manually. |

Watch progress when using `--no-wait`:

```sh
isd logs isd-controller -f
```
