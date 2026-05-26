---
title: Rotate the controller CA
description: Replace controller trust material by rebuilding enrollment.
---

# Rotate the controller CA

Automatic CA rotation is not a one-command workflow yet. The safe operator path is to preserve state, rebuild trust material in a controlled window, and re-enroll hosts.

Before touching trust material, take a backup:

```sh
isd backup --out ./backups/pre-ca-rotation.tgz.age
```

For a small lab fleet, the practical recovery path is:

1. Schedule downtime for route changes and agent re-enrollment.
2. Back up the current controller state.
3. Bring up a replacement controller with `isd init` in the target context.
4. Run `isd join-token` and re-enroll each host with the printed `isd join` command.
5. Redeploy stacks from source with `isd stack deploy`.
6. Remove the old cluster only after routes and agents are healthy.

Check route and host state before removing the old controller:

```sh
isd hosts ls
isd route ls
```

If the replacement fails, restore the encrypted backup into the original context with `isd restore`.
