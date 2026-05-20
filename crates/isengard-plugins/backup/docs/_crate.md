Age-encrypted SQLite backups for the Isengard controller.

Pipeline at a glance: WAL snapshot, age passphrase encryption, upload
to a pluggable destination, record history in `backup_runs`. The
plugin only reads from the controller's storage; the only rows it
writes are its own [`backup_runs`] history.

# Scheduler

[`spawn_scheduler`] runs a forever loop:

1. Load [`config::BackupConfig`] from the settings table.
2. Sleep until the next run is due
   (`last_successful_backup_run + interval`).
3. Fire [`runner::BackupRunner::run_once`].

When the plugin is disabled or no destination is configured the loop
sleeps 60s and re-checks, so flipping the toggle in the UI takes
effect on the next cycle without a controller restart.

# Snapshot

[`snapshot::create_snapshot`] checkpoints the WAL (`TRUNCATE` form,
which also resizes the WAL file back to zero) and copies the live DB
file under an IMMEDIATE-tx lock so no new writer can begin during the
copy. The result is byte-identical to the live DB at snapshot time
and lands in the system temp dir; the returned `NamedTempFile` is
unlinked on drop.

# Encryption

[`encrypt::encrypt_with_passphrase`] uses age's `scrypt` recipient
(PBKDF2). The passphrase is supplied via
`ISENGARD_BACKUP_PASSPHRASE`; the DB persists only the
[`encrypt::passphrase_fingerprint`] (first 12 hex chars of SHA-256)
so the UI can confirm the running controller's passphrase matches
the one stored at setup. X25519 recipients are deferred until SaaS
escrow earns them.

# Destinations

Two backends ship in 11a:

- [`destination::LocalDestination`][] writes
  `root/prefix/<name>` on the controller host.
- [`destination::S3Destination`][] is a hand-rolled SigV4-S3
  PUT/GET/LIST/DELETE against any S3-compatible endpoint
  (Cloudflare R2, AWS S3, Wasabi, B2, MinIO). No `aws-sdk-s3`
  dependency: the tree stays small.

# Retention

[`runner::BackupRunner::prune_retention`] lists every object the
destination is responsible for, sorts newest-first by name (names
embed a UTC timestamp, so lexical sort is chronological), and
deletes everything past `retention_keep`. Failure during prune logs
and doesn't fail the run: old objects sticking around is harmless.

# Restore

[`restore::restore_from_destination`] download, decrypts, validates,
then performs a two-rename atomic swap:

```text
mv  isengard.db       isengard.db.bak.<utc>
mv  restored-tmp.db   isengard.db
```

Either both succeed or we revert by renaming the backup back. The
previous DB stays on disk as `.bak.<ts>` for manual undo, never
silently deleted. The WAL and SHM siblings of the displaced file
move with it so SQLite's recovery doesn't apply them on top of the
restored bytes.

A successful restore replaces the live DB file, which means the
`running` row inserted on entry now lives in the renamed `.bak.<ts>`.
The restore opens a fresh [`Inventory`] against the new file and
inserts a final `success` row there; the `.bak.<ts>` keeps its
`running` row as a forensic trail.
