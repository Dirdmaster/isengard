Restore-from-destination flow.

Pipeline: download encrypted blob, decrypt with passphrase, validate
the bytes are a real SQLite database, rename the current DB to a
`.bak.<ts>` sibling, move the restored bytes into the original path,
open a fresh `Inventory` against the new file (which re-runs
migrations forward over the snapshot's schema). Each step is a
recorded transition on the `restore_runs` row created at entry;
failures land the row in `failed` state and (best-effort) revert the
swap so the controller is never left pointing at a half-replaced
file.

# Atomic swap

Two ordered renames give atomicity:

```text
mv  isengard.db       isengard.db.bak.<utc>
mv  restored-tmp.db   isengard.db
```

Either both succeed or the code reverts by `mv isengard.db.bak.<utc>
isengard.db`. The previous DB stays on disk as `.bak.<ts>`,
**never** silently deleted: the operator keeps a manual undo
trail even after a successful restore.

# WAL and SHM siblings

The displaced live DB has WAL and SHM siblings (`isengard.db-wal`,
`isengard.db-shm`) that, left in place, would be applied on top of
the snapshot bytes by SQLite's recovery logic, undoing the restore.
The code moves them next to the `.bak.<ts>` (so the forensic trail
stays consistent) and deletes the live-side siblings.

# Recording success after the swap

A successful restore replaces the live DB file, which means the
`running` row inserted on entry now lives in the renamed `.bak.<ts>`.
The function opens a fresh `Inventory` against the new file (which
also runs `sqlx::migrate!` forward) and inserts a final `success`
row there. The `.bak.<ts>` file keeps its `running` row as a
forensic trail.

# Dry-run

`dry_run = true` performs the download, decrypt, and validate steps
but skips the on-disk swap, `.bak.<ts>` rename, and migrations. The
`restore_runs` row still records the dry-run outcome so the UI can
report verification success without changing state.
