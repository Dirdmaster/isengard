---
title: isd restore
description: Reference for the `isd restore` subcommand.
---

# `isd restore`

Restore the controller state volume from an encrypted backup.

```sh
isd restore ./backups/lab.tgz.age
```

Sources can be a filesystem path, a Docker volume object, or an S3 object:

```sh
isd restore ./backups/lab.tgz.age
isd restore volume:isengard-backups/lab.tgz.age
isd restore s3://backups/isengard/lab.tgz.age
```

`restore` decrypts the backup and extracts it into `isd-controller-state`. It refuses to overwrite a populated state volume unless you pass `--overwrite`.

Use a passphrase file when running non-interactively:

```sh
isd restore ./backups/lab.tgz.age --passphrase-file ./age-passphrase.txt
```

Run `isd uninit --backup-first` before restoring over an existing cluster unless you are working in a throwaway context.
