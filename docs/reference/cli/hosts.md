---
title: isd hosts
description: Reference for the `isd hosts` subcommand.
---

# `isd hosts`

Inspect hosts enrolled with the controller.

## List hosts

```sh
isd hosts ls
```

The table shows each host id, reported hostname, and enrollment time.

Render JSON for scripts:

```sh
isd hosts ls --format json
```

Show full host ids instead of short suffixes:

```sh
isd hosts ls --full-id
```

Use this command after `isd init` or `isd join` to confirm that agents are reporting back to the controller.
