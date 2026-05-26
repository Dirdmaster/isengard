---
title: Recover from a bad deploy
description: Roll a stack back to a known-good compose file.
---

# Recover from a bad deploy

When a deploy breaks a service, first separate route health from container health.

```sh
isd route ls
isd stack ps observability
isd logs grafana --tail 100
```

If the compose file is wrong, check the last known-good version before redeploying it:

```sh
isd stack doctor ./compose.yaml
isd stack diff ./compose.yaml
isd stack deploy ./compose.yaml
```

If the controller state itself is damaged, restore from the most recent encrypted backup:

```sh
isd uninit --backup-first
isd restore ./backups/lab.tgz.age --overwrite
isd init
```

Do not use `--overwrite` unless you intend to replace the current controller state volume.
