---
title: isd backup
description: Reference for the `isd backup` subcommand.
---

# `isd backup`

Create an encrypted backup of the controller state volume.

```sh
isd backup
```

By default the backup lands in the current directory with an `iso-<context>-<timestamp>.tgz.age` name. The command streams the `isd-controller-state` Docker volume through `tar`, encrypts it with age, and writes the encrypted archive to the chosen destination.

Common destinations:

```sh
isd backup --out ./backups/lab.tgz.age
isd backup --to volume:isengard-backups
isd backup --to s3://backups/isengard/lab.tgz.age
```

Passphrase resolution order:

1. `--passphrase-file <path>`
2. `ISENGARD_BACKUP_PASSPHRASE`
3. Stored passphrase in `~/.config/isd/backup.toml`
4. Interactive prompt, saved under the current context

Keep the passphrase outside the host being backed up. Without it, the backup cannot be restored.
