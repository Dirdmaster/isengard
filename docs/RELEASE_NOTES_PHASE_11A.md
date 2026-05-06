# Phase 11A: Backup Foundation

Closes Dirdmaster/isengard#51.

Phase 11A ships the controller-side backup pipeline: snapshot the SQLite state, encrypt it with age, push it to S3-compatible storage (or a local path), keep N copies, surface status in the dashboard. Restore lands in 11B.

## What you get

- A new `/settings?tab=backup` page in the dashboard. Three-step setup modal:
  1. Pick a destination (Cloudflare R2 / generic S3 endpoint, or a local filesystem path).
  2. Set a passphrase. The dashboard derives a 12-char SHA-256 fingerprint and discards the value. The runtime controller reads the same passphrase from the `ISENGARD_BACKUP_PASSPHRASE` env var.
  3. Pick a schedule (hourly / 6h / daily / weekly) + retention count (default keep last 14).
- Snapshots are byte-identical SQLite copies (PRAGMA wal_checkpoint(TRUNCATE) + IMMEDIATE-tx file copy).
- Each snapshot is encrypted with `age` (passphrase-only in 11A; X25519 keypair recipients land later).
- Uploads are timestamped: `snapshot-YYYYMMDDTHHMMSSZ.db.age`. Lexical sort = chronological.
- Retention prunes anything past `retention_keep` newest objects after a successful upload.
- A history table shows the most recent 30 runs, status, object name, size, and any error.

## Operator setup walkthrough (Cloudflare R2)

1. Create an R2 bucket (`isengard-backups`) and an API token scoped to that bucket with read+write+delete.
2. Pick a strong passphrase. Store it in your password manager. **Lost passphrase = lost backups.**
3. Set the env var on the controller process:
   ```
   ISENGARD_BACKUP_PASSPHRASE=<paste it here>
   ```
   On a Docker-deployed controller, add `-e ISENGARD_BACKUP_PASSPHRASE=...` to the `docker run` command.
4. Restart the controller (or systemctl reload, or however your deployment picks env updates).
5. Open the dashboard at `Settings > Backup`. Click **Get started**:
   - Step 1: pick **S3 / R2**. Endpoint: `https://<account-id>.r2.cloudflarestorage.com`. Bucket: `isengard-backups`. Region: `auto`. Prefix: `controllers/prod` (or whatever namespacing you want). Access key id + secret access key from the R2 token you just minted.
   - Step 2: paste the same passphrase you exported in step 3. The fingerprint preview should match the value the running controller reports back on the next reload.
   - Step 3: pick a schedule. Defaults are fine (daily, keep 14).
6. Click **Save backup config**. Then **Run now** in the status panel to verify the round trip works.
7. The runs table should show `success` with a non-zero size.

## Upgrade notes for self-hosters

- Migration `0019_backup_config.sql` adds a `backup_runs` table. SQLite migrations are forward-only; no operator action needed.
- The new `backup` plugin auto-loads. With no config it stays idle (the scheduler sleeps and wakes every minute to re-check).
- The `ControllerHandles` struct gained a `db_path: PathBuf` field. If you maintain a fork that constructs `ControllerHandles` directly (you don't, unless you're me), add the field.

## Decisions

| Choice | What we did | Why |
|---|---|---|
| S3 client | `reqwest` + hand-rolled SigV4 (`hmac` + `sha2`) | `aws-sdk-s3` pulled 80+ transitive crates for what is fundamentally PUT/GET/LIST/DELETE. Hand-rolled signing fits in 100 lines. Revisit if multipart uploads or presigned URLs are ever needed. |
| Encryption | `age` crate, passphrase (scrypt) only | Single binary, no GPG, simple. X25519 recipient flow is in the `age` crate but unused for 11A; lands when SaaS escrow needs to manage keys per controller. |
| Scheduler | Plain interval timer | Phase 9D's window evaluator was deferred. An interval timer is correct enough until cron-style windows ship. |
| Retention | LRU keep-N (default 14) | The grandfather-father-son matrix in the source design is more storage hygiene than functional. Flat keep-N covers the common case; users can crank it up. |
| Passphrase storage | DB stores fingerprint only; controller reads passphrase from env var | The DB never holds the secret. The dashboard sees it only briefly during the PUT request to compute the fingerprint and is discarded. |

## What 11A does not ship (and 11B will)

- **Restore flow.** Pick a snapshot, paste the passphrase, swap the DB atomically, run pending migrations, restart listeners.
- **age X25519 recipients.** Operator-managed public keys instead of passphrase. Needed for SaaS escrow.
- **Manifest.json** at the bucket root. Avoids the LIST round-trip during snapshot browse and tracks schema version per snapshot.
- **GFS retention matrix.** Per-tier hourly/daily/weekly/monthly counts.
- **Provider preset polish.** R2 + Custom is enough for 11A; B2/AWS/Wasabi/MinIO presets are layout/copy work for 11I.

## Test summary

- 7 storage DAO tests (`backup_runs`).
- 8 snapshot integrity tests.
- 8 encryption tests (round-trip, fingerprint determinism, wrong-passphrase failure, empty-passphrase rejection).
- 5 LocalDestination tests + 5 S3Destination wiremock tests.
- 3 end-to-end pipeline tests (snapshot -> encrypt -> upload -> download -> decrypt round-trip; passphrase-missing failure; retention pruning).
- 9 dashboard endpoint tests.
- Workspace clippy clean, fmt clean, cargo deny clean.
