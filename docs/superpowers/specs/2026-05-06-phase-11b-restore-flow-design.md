# Phase 11B Restore Flow, Design

Closes Dirdmaster/isengard#52. Source design: `1 Projects/Isengard/Backup & Restore.md` in the vault (Restore section).

## Scope

Phase 11B closes the loop opened by 11A. Snapshots are stored encrypted on a destination; 11B downloads them, decrypts, verifies the bytes, atomically swaps the live SQLite file, runs forward migrations, and records the outcome. The UI is "the scariest UI in the dashboard": multi-step modal with destructive confirms.

What ships:

- `restore` module in the existing `isengard-plugin-backup` crate.
- `restore_runs` table (migration 0023) tracking restore attempts.
- 3 new REST endpoints under `/api/v1/backup/*`: trigger restore, list restore runs, describe a backup run for the UI to verify.
- `BackupRestoreModal.vue` plus a "Restore from backup" CTA in `BackupSettings.vue`.
- 6+ restore integration tests, 8+ REST tests, an end-to-end snapshot+restore round-trip.

What is deferred:

- **Cross-version downgrade detection.** 11B treats migrations as forward-only (sqlx panics on a backwards target schema). A friendlier "this snapshot is from a newer controller" message lands later.
- **Manifest browsing.** 11B reads the destination's `list()` directly (the LRU keep-N pool stays small). A bucket-root manifest.json is still 11+.
- **Multi-stage progress reporting.** 11B's restore is synchronous (response returns when the swap is complete or rolled back). A progress stream is overkill for the typical sub-30s controller DB.

## Architecture

### New crate file

`crates/isengard-plugins/backup/src/restore.rs` exposes:

```rust
pub struct RestoreOutcome {
    pub run_id: RestoreRunId,
    pub source_object: String,
    pub restored_at: DateTime<Utc>,
    pub previous_db_backup_path: PathBuf,
    pub bytes_restored: u64,
}

pub enum RestoreError { /* download, decrypt, verify, swap, migrate, storage, io */ }

pub async fn restore_from_destination(
    inv: &Arc<Inventory>,
    pool: &SqlitePool,
    db_path: &Path,
    dest: &dyn BackupDestination,
    object_name: &str,
    passphrase: &str,
    dry_run: bool,
) -> Result<RestoreOutcome, RestoreError>;
```

Steps:

1. `inv.insert_restore_run` records the attempt with `status = running`.
2. `dest.download(object_name)` pulls the encrypted blob.
3. `decrypt_with_passphrase` decrypts.
4. Validate the decrypted bytes are a real SQLite database: write to a temp file, open via `SqliteConnectOptions::create_if_missing(false)`, run `SELECT 1`. Reject anything that fails.
5. If `dry_run`, transition the run row to `success` and return without touching disk further.
6. Drain via `pool.close().await` (waits for in-flight transactions).
7. Atomic swap: rename `db_path` to `db_path.bak.<UTC-iso-basic>` then move the temp file to `db_path`. If the move fails, restore the original via the `.bak.<ts>` path.
8. Open a fresh `Inventory` against the new file (this re-runs `sqlx::migrate!` so any newer-schema migrations apply on top of the snapshot).
9. Mark the restore run as `success` with the `previous_db_backup_path`.
10. Return the `RestoreOutcome`.

### Atomic swap mechanism

We rely on POSIX rename semantics: `std::fs::rename` is atomic on the same filesystem. Two ordered renames:

```
mv  isengard.db       isengard.db.bak.20260506T101530Z
mv  restored-tmp.db   isengard.db
```

If step 1 succeeds but step 2 fails, we attempt to revert: `mv isengard.db.bak.<ts> isengard.db`. We **never delete the previous DB silently**; the `.bak.<ts>` file stays on disk so an operator can manually undo.

WAL siblings (`isengard.db-wal`, `isengard.db-shm`) are renamed alongside the main file when present. A snapshot is byte-identical to the main DB only (the WAL was already checkpointed in 11A), so the WAL files of the running controller can be removed safely after the swap.

### Controller drain

Calling `pool.close().await` on the backup plugin's pool waits for in-flight transactions to commit / roll back. The dashboard's `Inventory` pool has its own connection set; for v1 we accept that the dashboard's pool keeps a stale handle to the **old** file briefly (SQLite reopens transparently on next query when the file's on-disk inode has changed under it).

A future iteration can:
- Add a `controller.drain` event that pauses gRPC + HTTP listeners.
- Rebuild `ControllerHandles.inventory` post-restore.

For 11B we surface the recommendation in the UI ("Restart the controller after a restore for a clean state") and emit `controller.restored` on the bus so any subscriber can react.

### Migration policy after restore

After the swap, we open a fresh `Inventory::open` against the restored file. This calls `sqlx::migrate!`, which is forward-only: any migrations newer than what the snapshot was taken with are applied. If a migration fails (rare; should not happen across point releases), we mark the restore run as `failed` and leave the `.bak.<ts>` in place. The operator can manually swap back.

### Storage: `restore_runs`

Migration `0023_restore_runs.sql`:

```sql
CREATE TABLE restore_runs (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    source_object            TEXT NOT NULL,
    source_backup_run_id     INTEGER,
    started_at               TEXT NOT NULL,
    finished_at              TEXT,
    status                   TEXT NOT NULL CHECK (status IN ('running','success','failed')),
    previous_db_backup_path  TEXT,
    bytes_restored           INTEGER,
    error                    TEXT
);
CREATE INDEX idx_restore_runs_started_at ON restore_runs(started_at DESC);
```

DAO mirror of `backup_runs`: insert, finish-success, finish-failed, list. We pick a separate table (not extending `backup_runs`) because the operations are semantically distinct: a restore points at a backup, has different success metadata, and the UI lists them separately.

### REST endpoints

| Method | Path | Body / query | Response |
|---|---|---|---|
| POST | `/api/v1/backup/restore` | `{ object_name, passphrase, dry_run }` | `200 { outcome }` or `4xx { error }` |
| GET  | `/api/v1/backup/restore-runs?limit=` | | `[ RestoreRunDto ]` |
| GET  | `/api/v1/backup/runs/:id/manifest` | | `{ object_name, size_bytes, started_at, fingerprint_match }` |

The manifest endpoint is the dashboard's pre-flight check: before showing the confirm screen, the UI fetches the manifest for the picked run, compares the controller's stored `passphrase_fingerprint` against what the operator typed (client-side hashed), and warns on mismatch.

The restore endpoint runs synchronously. A typical controller DB restores in <30s including download + decrypt + swap + migrate; the HTTP timeout window covers it. The endpoint returns 200 on success (with the `RestoreOutcome`), 4xx on user error (wrong passphrase, missing object, dry run validation), 5xx on infrastructure failure (network, disk).

### UI

`BackupRestoreModal.vue`:

- **Step 1**: pick a run from the existing `runs` history, filtered to status `success`, sorted newest-first.
- **Step 2**: paste the passphrase. The UI hashes it client-side (Web Crypto subtle digest, same algo as `passphrase_fingerprint`) and compares to the configured fingerprint. If they don't match, "Continue" stays disabled with a hint.
- **Step 3**: "What happens" summary: the snapshot timestamp, the destination object, the `.bak.<ts>` path the current DB will land at, and a warning that active connections will drop.
- **Step 4**: "Type RESTORE to confirm" text input. The "Restore" button is red and only enables when the literal phrase matches.

A status panel beneath the BackupSettings status section shows the most recent restore attempt (if any), its duration, and the fallback `.bak.<ts>` path.

### Test surface

- `tests/restore.rs`: 6+ integration tests
  - Download then decrypt round-trip succeeds.
  - Wrong passphrase fails with a typed error.
  - Missing object fails with a typed error.
  - Garbage decrypted bytes (not a SQLite DB) fail validation.
  - Atomic swap creates `.bak.<ts>` and the new file matches the snapshot bytes.
  - Atomic swap rollback: if the second rename fails (simulated), the original file is restored.
  - Migrations run on the restored file (apply a fresh schema after restoring an older one).
  - End-to-end: snapshot via 11A then restore via 11B, verify a seeded row matches.

- `crates/isengard-plugins/dashboard/src/backup.rs`: 8+ endpoint tests covering POST restore (happy path, missing object, missing runner), GET restore-runs (empty, populated, ordering), GET manifest (existing, missing).

## Open questions

| Question | Decision |
|---|---|
| Sync vs streaming response | Sync. Simpler; sub-30s typical. |
| Should 11B also re-run plugin `init` after restore? | No. Plugins are stateless across restarts; operators are advised to restart the controller. |
| What if `.bak.<ts>` already exists? | Append a `-N` suffix until unique. We never overwrite. |
| Do we delete the `.bak.<ts>` ever? | No. Operator-managed. We document the location in the API response and the UI. |
