# Phase 11A Backup Foundation, Plan

Spec: [`2026-05-06-phase-11a-backup-foundation-design.md`](../specs/2026-05-06-phase-11a-backup-foundation-design.md). Branch: `feat/phase-11a`. Worktree: `~/Projects/isengard/.worktrees/phase-11a`. Migration slot: `0019`. Issue: Dirdmaster/isengard#51.

Implementer model: Opus for every commit (per session preference).

## Standing self-review (every commit)

1. `cargo build --workspace`
2. `cargo test --workspace --exclude isengard-plugin-backup` for non-plugin slices, full workspace test for plugin slices.
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo fmt --check`
5. Grep new files for em dash (U+2014) and en dash (U+2013); zero tolerance.
6. Migration up runs cleanly on a fresh in-memory DB.
7. `bun run build` in `crates/isengard-plugins/dashboard/web` for UI commits.
8. Commit subject references issue #51.

## Commit list

### C1, Storage migration 0019 + BackupRun DAO

- `crates/isengard-storage/migrations/0019_backup_config.sql`: `backup_runs` table.
- `crates/isengard-storage/src/backup_run.rs`: `BackupRun`, `BackupRunStatus`, `InsertBackupRun` + DAO methods on `Inventory`.
- `crates/isengard-storage/src/lib.rs`: register module.
- `crates/isengard-storage/tests/backup_run_dao.rs`: 6 tests (insert, finish-success, finish-failed, list-recent ordering, list-recent limit, status enum round-trip).

### C2, Backup plugin scaffold + snapshot module

- `crates/isengard-plugins/backup/Cargo.toml`.
- `crates/isengard-plugins/backup/src/lib.rs`: `Plugin` skeleton (init/start/stop), `inventory::submit!`.
- `crates/isengard-plugins/backup/src/snapshot.rs`: `create_snapshot` with WAL checkpoint + lock-and-copy fallback to `VACUUM INTO` for in-memory pools.
- `crates/isengard-plugins/backup/tests/snapshot.rs`: 8 tests.
- Wire into the workspace `Cargo.toml` + `crates/isengard/Cargo.toml` + `main.rs` force-link.

### C3, Encryption module (age + fingerprint)

- `crates/isengard-plugins/backup/src/encrypt.rs`: `encrypt_with_passphrase`, `decrypt_with_passphrase`, `passphrase_fingerprint`.
- `crates/isengard-plugins/backup/tests/encrypt.rs`: 4 tests.

### C4, Destination trait + LocalDestination + S3Destination

- `crates/isengard-plugins/backup/src/destination.rs`: trait + two impls.
- `crates/isengard-plugins/backup/tests/destination_local.rs`: round-trip.
- `crates/isengard-plugins/backup/tests/destination_s3.rs`: wiremock harness.

### C5, Config + Runner + Scheduler

- `crates/isengard-plugins/backup/src/config.rs`: `BackupConfig`, load/save via `Inventory`.
- `crates/isengard-plugins/backup/src/runner.rs`: `BackupRunner::run_now()`, retention pruning.
- `crates/isengard-plugins/backup/src/lib.rs`: scheduler loop wired into `Plugin::start`.
- `crates/isengard-plugins/backup/tests/end_to_end.rs`: snapshot → encrypt → upload-local → download → decrypt round-trip.

### C6, REST endpoints

- `crates/isengard-plugins/dashboard/src/backup.rs`: router with 4 routes.
- `crates/isengard-plugins/dashboard/src/lib.rs`: nest the router.
- `crates/isengard-plugins/dashboard/Cargo.toml`: depend on `isengard-plugin-backup`.
- 8 endpoint tests covering happy paths, secret masking, validation errors.

### C7, UI settings backup tab + 3-step modal + runs table

- `crates/isengard-plugins/dashboard/web/components/BackupSettings.vue`.
- `crates/isengard-plugins/dashboard/web/components/BackupSetupModal.vue`.
- `crates/isengard-plugins/dashboard/web/pages/settings/index.vue`: register tab.
- Storybook-style local fetches via `$fetch`.

### C8, Wrap-up

- `design/pages/settings-backup.md`: status flipped to shipped, implementation status block.
- `docs/RELEASE_NOTES_PHASE_11A.md`: operator-facing R2 walkthrough.
- Workspace lint/test/deny gate sweep.
- Push branch, open PR with body "Closes #51".
