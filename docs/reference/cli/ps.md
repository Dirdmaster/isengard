---
title: isd ps
description: Reference for the `isd ps` subcommand.
---

# `isd ps`

List containers visible from the current Docker context.

```sh
isd ps
```

By default the table hides healthy Isengard system containers so the operator view stays focused on managed workloads. Failed or stopped system containers still appear because they need attention.

Useful flags:

| Flag | Meaning |
|---|---|
| `-a`, `--all` | Show stopped containers and healthy Isengard infrastructure. |
| `--no-trunc` | Show full container ids and commands. |
| `--format json` | Emit JSON for scripts. |
| `--filter KEY=VALUE` | Filter by `host`, `stack`, `service`, or `state`. |
| `--host <name>` | Show one host. |
| `--no-group` | Force a flat table. |

`isd ps` also refreshes the local selector cache used by commands such as `isd logs '#2'` and `isd restart '#2'`.
