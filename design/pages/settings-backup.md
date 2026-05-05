---
type: design
kind: page-spec
status: phase-11-pending
status_note: "Backup & Restore is Phase 11"
created: 2026-05-03
updated: 2026-05-05
tags:
  - design
  - page
  - settings
  - backup
---

# Settings · Backup

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
