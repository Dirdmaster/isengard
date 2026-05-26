---
title: isd init
description: Reference for the `isd init` subcommand.
---

# `isd init`

Bootstrap a controller and the first agent on the current Docker context.

```sh
isd init
```

Use a specific Docker context when the target host is not the active one:

```sh
isd --context lab init
```

`init` creates the Isengard state volumes, starts `isd-controller`, mints the first join token, and starts `isd-agent` on the same host. It is safe to run again when the cluster containers already exist.

After bootstrapping, check the fleet:

```sh
isd ps
isd hosts ls
```

To add more hosts, run `isd join-token` from the controller context and paste the printed `isd join` command for the target context.
