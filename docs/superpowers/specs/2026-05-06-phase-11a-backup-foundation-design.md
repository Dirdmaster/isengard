# Phase 11A Backup Foundation, Design

Closes Dirdmaster/isengard#51. Source design: `1 Projects/Isengard/Backup & Restore.md` in the vault.

## Scope

Phase 11A delivers the minimum end-to-end backup pipeline: take a SQLite snapshot, encrypt it, push it to S3-compatible storage, prune old copies, expose status via REST + UI. Restore is intentionally out of scope (Phase 11G).

What ships:

- `isengard-plugin-backup` crate with SQLite WAL snapshot, age passphrase encryption, pluggable destinations (S3-compatible + local file), interval scheduler, LRU retention.
- Storage migration `0019_backup_config.sql` adding `backup_runs` table for run history. Configuration itself rides the existing `settings` key/value table.
- REST endpoints under `/api/v1/backup/*`: get/put config, run-now, list runs.
- Settings UI: new "Backup" tab with 3-step setup modal (destination, passphrase, schedule) + status panel.
- Tests: unit (snapshot, age round-trip, retention), wiremock-based S3 upload, REST contract, end-to-end snapshot+encrypt+upload+download+decrypt.

What is deferred:

- **Restore flow** (download + decrypt + atomic swap + migrations): Phase 11B.
- **age X25519 keypair recipients**: 11A uses passphrase-only encryption (PBKDF2 via the `age` crate's `scrypt::Recipient`). Recipient-style flow lands later when SaaS escrow is needed.
- **Grandfather-father-son rotation**: 11A keeps a flat "last N" retention. The matrix in the source design is 11+.
- **Manifest.json**: 11A lists by S3 prefix listing. Manifest is 11B (needed for restore browsing).
- **Provider presets beyond R2**: B2, AWS, Wasabi, MinIO are all S3-API compatible and work via the generic endpoint field, but the UI only ships R2 + Custom presets in 11A.

## Architecture

### Crate layout

`crates/isengard-plugins/backup/`:
- `Cargo.toml`
- `src/lib.rs` : `Plugin` impl, scheduler task, `inventory::submit!`.
- `src/snapshot.rs` : WAL checkpoint + lock-and-copy via the SQLite pool.
- `src/encrypt.rs` : age passphrase encrypt/decrypt + key fingerprint helper.
- `src/destination.rs` : `BackupDestination` trait, `S3Destination`, `LocalDestination`.
- `src/config.rs` : `BackupConfig` struct + load/save via `Inventory::settings`.
- `src/runs.rs` : `BackupRun` struct + DAO over `backup_runs` table.
- `tests/snapshot.rs`, `tests/encrypt.rs`, `tests/destination_local.rs`, `tests/destination_s3.rs`, `tests/end_to_end.rs`.

### Snapshot mechanism

```rust
pub async fn create_snapshot(pool: &SqlitePool, db_path: &Path) -> Result<NamedTempFile> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(pool).await?;
    let mut tx = pool.begin().await?; // BEGIN IMMEDIATE
    let tmp = NamedTempFile::new()?;
    std::fs::copy(db_path, tmp.path())?;
    tx.rollback().await?; // release the IMMEDIATE lock
    Ok(tmp)
}
```

Test seam: when `db_path` is `:memory:` (in-memory test pool), the snapshot helper copies via `VACUUM INTO` instead of file copy. This keeps the in-memory test path realistic without dragging the rusqlite Backup API into the dependency tree.

### Encryption

`age` crate, passphrase-only for v1:

```rust
let recipient = age::scrypt::Recipient::new(passphrase.into());
let encrypted = age::encrypt(&recipient, &snapshot_bytes)?;
```

The passphrase is provided to the controller via env var `ISENGARD_BACKUP_PASSPHRASE`. The DB stores only a SHA-256 fingerprint of the passphrase (12-hex-char prefix) so the UI can show "key fingerprint matches what is configured" without ever reading or persisting the passphrase. Operator manages the secret. No recovery service.

Decryption (used in tests and the future restore flow) uses `age::scrypt::Identity` plus an explicit `set_max_work_factor` cap so test runs stay sub-second.

### Destinations

```rust
#[async_trait]
pub trait BackupDestination: Send + Sync {
    async fn upload(&self, name: &str, bytes: &[u8]) -> Result<()>;
    async fn list(&self) -> Result<Vec<RemoteObject>>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn download(&self, name: &str) -> Result<Vec<u8>>; // used by tests + 11B
}
```

Implementations:

- `LocalDestination { root: PathBuf, prefix: String }`: filesystem writes under `root/prefix/`.
- `S3Destination { endpoint, region, bucket, prefix, access_key_id, secret_access_key }`: built on `reqwest` + manual SigV4 signing.

#### S3 client choice

`aws-sdk-s3` was the obvious pick from the source design but pulls 80+ transitive crates (aws-config, aws-runtime, hyper-rustls, smithy-*, etc.). For a plugin that only does PUT + GET + LIST + DELETE, a hand-rolled SigV4 path keeps build times and `cargo deny` surface manageable. The `aws-sigv4` crate covers the signing primitive and adds ~5 deps. Decision: `reqwest` + `aws-sigv4` for 11A. Document in release notes; revisit if a complex feature (multipart, presigned URLs) lands.

### Scheduler

A simple interval timer in 11A. The plugin spawns a background task that:

1. Reads `BackupConfig` from settings.
2. If `enabled`, computes the next run time as `last_run_at + interval_secs` (or `now + interval_secs` when never run).
3. Sleeps until that time, fires `run_backup`, repeats.

The interval is in seconds with a soft floor of 60s. Default: `86400` (daily). The cron-style window evaluator from Phase 9D was deferred (see policy/mod.rs note about phase 9h); when it lands, the scheduler can swap to it without changing the snapshot/encrypt path.

`run_backup` is also exposed via the `POST /api/v1/backup/run-now` endpoint. The scheduler and the REST handler share the same code path through an `Arc<BackupRunner>`.

### Retention

After a successful upload, the runner calls `destination.list()` and deletes everything beyond the most recent `retention_keep` (default 14, sorted by name; names embed `YYYYMMDDTHHMMSSZ`). LRU eviction. Nothing fancy.

### Run history

New `backup_runs` table:

```sql
CREATE TABLE backup_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    status      TEXT NOT NULL CHECK (status IN ('running','success','failed')),
    object_name TEXT,
    size_bytes  INTEGER,
    error       TEXT
);
CREATE INDEX idx_backup_runs_started_at ON backup_runs(started_at DESC);
```

Configuration lives in the existing `settings` table under keys `backup.config.*`, identical to how the notifier and networking plugins persist config. Storing the encryption key fingerprint there is fine; the actual key never touches the DB.

## REST endpoints

| Method | Path | Body / query | Returns |
|---|---|---|---|
| GET | `/api/v1/backup/config` | none | `BackupConfigDto` (passphrase fingerprint only, never raw secrets) |
| PUT | `/api/v1/backup/config` | `BackupConfigDto` (with `secret_access_key` write-only) | `204` |
| POST | `/api/v1/backup/run-now` | none | `202` + `{ run_id }` |
| GET | `/api/v1/backup/runs?limit=` | `limit` 1..200, default 30 | `Vec<BackupRunDto>` |

Secret fields (`secret_access_key`) are write-only: PUT accepts them but GET masks them as `***`. The passphrase is configured separately via the `ISENGARD_BACKUP_PASSPHRASE` env var; the dashboard's "Set passphrase" step writes its SHA-256 fingerprint to settings so the UI can show "matches/mismatches the running controller" without ever holding the secret.

## UI

New `BackupSettings.vue` component mounted as `backup` tab on `/settings`. Two states:

1. **Not configured** : the "Get started" CTA opens a 3-step modal:
   - Step 1: Destination (Provider preset, endpoint, bucket, prefix, access key id, secret access key).
   - Step 2: Encryption : show the env-var name + a passphrase input that the user pastes; the dashboard stores its fingerprint and prompts the operator to also set the env var. Friction is intentional: the controller never knows the secret unless the operator gave it via env var.
   - Step 3: Schedule : interval (hourly / daily / weekly) + retention count.
2. **Configured** : status panel: last run, next run, retention, manual "Run now" button, runs history table.

## Test plan

- 8+ unit tests in `tests/snapshot.rs` covering: WAL checkpoint runs, snapshot byte-identical to source on disk, lock holds + releases, in-memory `VACUUM INTO` path, snapshot reflects committed writes, snapshot does not include uncommitted writes (a single open WAL frame test), idempotent re-snapshot, snapshot fails cleanly on read-only fs.
- `tests/encrypt.rs`: passphrase round-trip, fingerprint determinism, decrypt with wrong passphrase fails, fingerprint of empty passphrase is empty.
- `tests/destination_local.rs`: upload+list+download+delete round-trip on a tempdir.
- `tests/destination_s3.rs`: wiremock harness : PUT receives correct path + signed Authorization header; GET returns the bytes; LIST is parsed; DELETE issues DELETE.
- `tests/end_to_end.rs`: snapshot → encrypt → upload to LocalDestination → download → decrypt → byte-identical.
- 8+ REST endpoint tests in `dashboard` plugin: GET/PUT config, secret masking, run-now, runs listing, validation errors.

## Hard rules respected

- No em dashes (U+2014) or en dashes (U+2013) in any new file.
- Encryption key never stored in DB; fingerprint only. Operator is the source of truth.
- All commits reference issue #51.
- `cargo deny` config updated to allow new advisory baseline if any new transitive lands; no new licenses added without justification.
