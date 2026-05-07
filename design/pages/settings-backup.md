---
type: design
kind: page-spec
status: shipped
status_note: "Phase 11A + 11B shipped the full backup pipeline including atomic-swap restore. Out: GFS retention matrix, age X25519 recipients, manifest.json, B2/AWS/Wasabi/MinIO presets."
created: 2026-05-03
updated: 2026-05-06
tags:
  - design
  - page
  - settings
  - backup
---

# Settings, Backup

## Implementation status (2026-05-06, Phase 11A + 11B)

Shipped (11A):
- Snapshot mechanism (PRAGMA wal_checkpoint(TRUNCATE) + IMMEDIATE-tx + std::fs::copy).
- Encryption: age passphrase via the `age` crate, fingerprint persisted (SHA-256 prefix), passphrase via env var.
- Destinations: LocalDestination (filesystem) and S3Destination (reqwest + hand-rolled SigV4). Tested wiremock-side for PUT/GET/LIST/DELETE.
- Interval scheduler (default daily). Re-reads config on every cycle so the enabled toggle is hot-pluggable.
- Retention: LRU keep-N (default 14). Pruned after every successful upload, best effort.
- REST: GET/PUT /api/v1/backup/config, POST /api/v1/backup/run-now, GET /api/v1/backup/runs.
- UI: Settings, Backup tab + 3-step setup modal (destination, passphrase, schedule) + status panel + runs table + Run-now button.

Shipped (11B):
- Restore module: download from destination, decrypt, validate the bytes parse as a real SQLite database (open + SELECT 1), atomic-swap (rename live -> .bak.<utc>, then move staged -> live), open a fresh Inventory which runs sqlx::migrate! forward over the snapshot's schema.
- Atomic-swap rollback: on second-rename failure, the original file is restored from .bak.<ts>. The previous DB is NEVER deleted silently; operators can manually undo.
- WAL/SHM siblings of the live path are moved aside before the swap so SQLite's recovery logic does not replay the old DB's WAL onto the snapshot bytes.
- Storage: `restore_runs` table (migration 0023) tracks each attempt (running / success / failed) with the previous_db_backup_path and bytes_restored.
- REST: POST /api/v1/backup/restore (sync), GET /api/v1/backup/restore-runs, GET /api/v1/backup/runs/:id/manifest (pre-flight info for the UI).
- UI: BackupRestoreModal four-step destructive flow (pick snapshot -> verify passphrase fingerprint -> review destructive change -> type the literal phrase RESTORE). Mounted in BackupSettings.vue with a prominent red button. A status panel surfaces the most recent restore attempt including the .bak.<ts> path.

Deferred (still 11+):
- age X25519 keypair recipients (passphrase-only for now).
- Grandfather-father-son retention matrix (flat keep-N for now).
- Bucket-root manifest.json (LIST works directly off prefix listing).
- Provider presets beyond R2 / Custom in the modal (B2, AWS, Wasabi, MinIO all already work via the generic S3 endpoint config).
- Cross-version downgrade detection (sqlx panics on a backwards target schema; a friendlier "this snapshot is from a newer controller" message lands later).

Configuration + status + restore for the controller's SQLite snapshot pipeline.

Source design: [[Backup & Restore]].

## Route

`/settings/backup`

## Sections

1. **Status panel** — last successful backup, next scheduled, retention policy summary, encryption key fingerprint
2. **Schedule** — cron expression (default daily 03:00 host TZ), retention policy editor (keep N + GFS rotation)
3. **Destination** — S3-compatible config (endpoint, bucket, region, access key, prefix). R2 quick-pick at top.
4. **Encryption** — age public key, "Save encryption key" button (escapes the secret outside the controller)
5. **Manual actions** — Backup now · Verify last backup · Restore from backup (the scariest button)
6. **History** — last 30 backup attempts with status / duration / size

## Components used

- `<TopBar />`
- `<PageHeader title="Settings" sub="Backup & Restore" />`
- `<SettingsTabs active="backup" />`
- `<BackupStatusCard />` — top-of-page status with green/amber/red dot
- `<DestinationCard />`
- `<EncryptionKeyCard />`
- `<BackupHistoryTable />`
- `<SaveEncryptionKeyModal />` — step-1 of setup; red warning + age key block + "I've saved this" confirm
- `<RestoreModal />` — danger flow: red border + key fingerprint match + 2 explicit confirms ("type RESTORE to continue")
- `<BottomBar />`

## States

- **Not configured** (no destination): yellow card "Set up backups to protect your controller state"
- **Configured + healthy**: green status, last backup ≤ 26h ago
- **Configured + degraded** (last backup failed): amber/red status with last error + Retry now
- **Backup running**: animated indicator + ETA
- **Restore in progress**: full-page modal blocks navigation, controller enters maintenance state

## Open questions

- ❓ Per-fleet encryption keys vs single? — single for v1, multi in v2 SaaS
- ❓ Show backup size growth over time chart? — defer
- ❓ Encryption key rotation flow? — defer; document manual procedure for now

## Related

- Concepts: `concepts/2026-05-03-settings-backup-v1.html` (TODO)
- Source: [[Backup & Restore]]

---

> Approvals tab is pending Phase 9 — not currently in TopBar.
