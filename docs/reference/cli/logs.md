---
title: isd logs
description: Reference for the `isd logs` subcommand.
---

# `isd logs`

Read container logs from the current Docker context.

```sh
isd logs whoami
```

Targets can be a container name, a container id, a `#N` index from `isd ps`, or a range such as `0-3`.

Useful flags:

| Flag | Meaning |
|---|---|
| `-f`, `--follow` | Stream new lines after the backfill. |
| `--tail <n>` | Number of lines from the end. Default: `200`. |
| `-t`, `--timestamps` | Include container log timestamps. |

Examples:

```sh
isd ps
isd logs '#2' --tail 50
isd logs whoami -f
```

Cross-host log streaming is not wired through the controller yet. If a target lives on another host, SSH to that host and run `isd logs` there.
