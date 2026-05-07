# Phase 11B: Restore Flow

Closes Dirdmaster/isengard#52.

Phase 11B closes the loop opened by 11A. Snapshots are downloaded from the configured destination, decrypted, validated, and atomically swapped into place. The live controller's previous database is preserved as a `.bak.<utc>` sibling so operators can undo manually if anything goes wrong.

## What you get

- New REST endpoints under `/api/v1/backup/`:
  - `POST /restore` (synchronous): body `{ object_name, passphrase, dry_run }`. Returns `{ run_id, source_object, restored_at, previous_db_backup_path, bytes_restored, dry_run }` on success.
  - `GET /restore-runs?limit=`: list past restore attempts.
  - `GET /runs/:id/manifest`: pre-flight info for the UI to verify the passphrase fingerprint.
- New "Restore from backup" red button on the `Settings > Backup` page. Opens a four-step destructive flow:
  1. Pick a successful snapshot from the runs history.
  2. Paste the passphrase. The dashboard hashes it locally (Web Crypto subtle digest) and compares to the controller's stored fingerprint. The Continue button stays disabled until the fingerprints match.
  3. Review the destructive change in plain English, including the `.bak.<utc>` path the current DB will land at. Optional: dry-run.
  4. Type the literal phrase `RESTORE` to enable the red button.
- Status panel surfaces the most recent restore attempt below the backup status: success / failure / progress, plus the `.bak.<ts>` path so operators always know where the previous DB went.
- New `restore_runs` table (migration 0023) records every attempt with the source object, previous-db backup path, bytes restored, and any error. SQLite migrations are forward-only; no operator action needed.

## Disaster-recovery walkthrough

Scenario: the controller host died, you have a fresh VM, and you want to bring back a controller from the offsite snapshots.

1. Provision the new host. Install the controller binary or pull the docker image (same version or newer than the controller that produced the backup).
2. Set `ISENGARD_BACKUP_PASSPHRASE` to the same passphrase you used during the original setup. Without this, the controller cannot decrypt your snapshots.
3. Start the controller. The DB file at `<state_dir>/isengard.db` will be empty (fresh) at this point.
4. Open the dashboard. Go to `Settings > Backup` and click `Get started` to re-create the destination config (R2 or whatever you used). Paste the same passphrase so the fingerprint matches what your existing snapshots were encrypted with.
5. Run a manual `Run now` first to confirm the destination credentials work end-to-end. (You'll briefly have a backup of the empty DB; retention will prune it later.)
6. Click the red `Restore from backup` button.
7. Pick the snapshot you want to restore (newest is usually the right answer).
8. Paste the passphrase. The fingerprint should match what was just shown after the round-trip in step 5.
9. Review the destructive change. The current DB (which is the freshly-seeded empty DB) will be saved at `<state_dir>/isengard.db.bak.<utc>` so you can confirm the swap happened.
10. Type `RESTORE` and click the red button. After ~5-30 seconds (depending on DB size and destination speed), the success panel shows the bytes restored and the `.bak.<ts>` path.
11. Restart the controller for a clean state. Agents reconnect automatically because their enrollment state is preserved in the snapshot.

If the dashboard reports a wrong-passphrase or invalid-snapshot error, the live DB was not touched (these checks run before the swap). Inspect the `restore_runs` history under `Settings > Backup` for the recorded failure.

## Operator tips

- **Pause the scheduler before restoring.** The backup runner takes an IMMEDIATE-tx lock on the live DB; if a snapshot fires during the restore swap window, the backup-side run will record a failure (which is harmless, but noisy). Toggle `Enabled` off in the backup config before clicking Restore, then back on after.
- **Restart the controller after the restore.** The active controller process keeps SQLite handles to the (now-renamed) old DB inode. SQLite reopens transparently in most cases, but a clean restart is safer.
- **Keep the `.bak.<ts>` files.** They are your manual-undo path. If a restore turned out to be the wrong snapshot, you can stop the controller, swap the file back manually (`mv isengard.db.bak.<ts> isengard.db`), and start the controller again.
- **Schema downgrades are not supported.** If you restore a v0.5 snapshot into a v0.3 controller binary, sqlx will panic on the unknown schema. Always use a controller binary at least as new as the one that produced the snapshot.

## Atomic-swap mechanism

Two ordered renames on the same filesystem:

```
mv  isengard.db                 isengard.db.bak.<utc>
mv  /tmp/restored-staged.db     isengard.db
```

If the second rename fails, we revert by moving the backup back to its original name. We never delete the previous DB silently.

WAL + SHM siblings of the live path (`isengard.db-wal`, `isengard.db-shm`) are moved alongside the main file before the swap; otherwise SQLite's recovery logic on the next open would replay the old DB's WAL onto the snapshot bytes, undoing the restore. (We discovered this the hard way during integration tests.)

## Decisions

| Choice | What we did | Why |
|---|---|---|
| Sync vs streaming response | Sync (one HTTP roundtrip). | Sub-30s typical for controller-sized DBs (<100 MB). Streaming progress is overkill. |
| New `restore_runs` table vs extending `backup_runs` | Separate table. | Different success metadata (the `.bak.<ts>` path is meaningful only for restores), different UI surface. |
| Where the success row lives post-swap | Insert a fresh row in the new DB after the swap. | The swap replaces the live DB with the snapshot, which has no record of the in-progress restore. The `.bak.<ts>` keeps its `running` row as a forensic trail. |
| Swap mechanism | `std::fs::rename` x2 with rollback. | POSIX rename is atomic on the same FS. SQLite's online-backup API would dragNN rusqlite's Backup interface into the dependency tree for marginal benefit. |
| Controller drain | None for v1. UI advises a restart. | Adding live-listener pause and `ControllerHandles.inventory` rebuild was scope creep. SQLite reopens transparently in the typical case. |
| Migrations after restore | `Inventory::open` re-runs `sqlx::migrate!` forward. | Idempotent and forward-only. Snapshots from an older controller version get migrated up automatically. |
| WAL/SHM handling | Renamed alongside the main file before swap. | Required: leftover WAL would corrupt the restored DB on next open. |

## What 11B does not ship

- **Cross-version downgrade detection.** Restoring a newer-schema snapshot into an older controller will surface as a sqlx error during the post-swap migration; a friendlier message ("snapshot from controller v0.5; current is v0.3 — downgrades not supported") lands later.
- **Live-listener drain.** The controller stays up during the restore; operators are advised to restart afterwards.
- **Bucket-root manifest.json.** Still 11+. The current LIST + per-run manifest is sufficient for v1.

## Test summary

- 4 storage DAO tests for `restore_runs` (insert, finish-success, finish-failed, list ordering).
- 9 restore integration tests (round-trip, dry-run, wrong passphrase, missing object, empty passphrase, garbage decrypted bytes, .bak preservation, recorded failed-then-success attempts, migrations apply).
- 2 inline unit tests for backup-path collision handling.
- 8 dashboard endpoint tests (restore: missing object, missing passphrase, runner-not-started; restore-runs: empty + populated; manifest: 404 / 409 / 200).
- Workspace clippy clean, fmt clean, cargo deny clean. `bun run build` clean.
