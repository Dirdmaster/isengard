---
title: isd uninit
description: Reference for the `isd uninit` subcommand.
---

# `isd uninit`

Remove the local Isengard controller and agent containers.

```sh
isd uninit
```

By default `uninit` preserves the state volumes:

- `isd-controller-state`
- `isd-agent-state`
- `isd-stacks`

That makes it possible to run `isd init` again or restore from a backup without losing fleet state.

Common flags:

| Flag | Meaning |
|---|---|
| `--backup-first` | Run `isd backup` before teardown. |
| `--yes` | Skip the confirmation prompt. |
| `--wipe-state` | Delete state volumes too. This is not recoverable without a backup. |

Use the safer teardown when you intend to rebuild the cluster:

```sh
isd uninit --backup-first
```
