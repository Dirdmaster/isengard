# Phase 11B Restore Flow, Implementation Plan

Spec: `docs/superpowers/specs/2026-05-06-phase-11b-restore-flow-design.md`
Issue: Dirdmaster/isengard#52
Branch: `feat/phase-11b` off `next`.

## Sequencing

Each step is one logical commit. Gates pass at every step.

### 1. Storage migration + DAO

Files:
- `crates/isengard-storage/migrations/0023_restore_runs.sql` (new).
- `crates/isengard-storage/src/restore_run.rs` (new): `RestoreRunStatus`, `RestoreRunId`, `RestoreRun`, plus `Inventory` impl methods (`insert_restore_run`, `finish_restore_run_success`, `finish_restore_run_failed`, `list_restore_runs`).
- `crates/isengard-storage/src/lib.rs`: re-export the new types.

Tests: 4 DAO unit tests inline in `restore_run.rs` (insert, success-finish, failed-finish, list ordering).

Gate: `cargo test -p isengard-storage`.

### 2. Restore module in backup plugin

Files:
- `crates/isengard-plugins/backup/src/restore.rs` (new).
- `crates/isengard-plugins/backup/src/lib.rs`: `pub mod restore`.
- `crates/isengard-plugins/backup/src/runner.rs`: expose a helper to build a `BackupDestination` from a settings-loaded `BackupConfig` (already private; promote it to `pub`).
- `crates/isengard-plugins/backup/Cargo.toml`: no new deps (everything we need is already in 11A's tree).

Public API:

```rust
pub struct RestoreOutcome { ... }
pub enum RestoreError { ... }

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

Plus a thin wrapper on `BackupRunner`:

```rust
impl BackupRunner {
    pub async fn restore_now(
        &self,
        object_name: &str,
        passphrase: &str,
        dry_run: bool,
    ) -> Result<RestoreOutcome, RestoreError>;
}
```

Tests: `crates/isengard-plugins/backup/tests/restore.rs` (new), 6+ integration tests:
- Round-trip: snapshot, then restore, verify rows match.
- Wrong passphrase produces `RestoreError::Decrypt`.
- Missing object produces a typed error.
- Garbage bytes after decrypt fail SQLite validation.
- Swap creates `.bak.<ts>` next to the original.
- Dry-run does not touch disk.
- Atomic swap rollback path (use a directory permission trick to force the second rename to fail; verify `.bak` reverts).

Gate: `cargo test -p isengard-plugin-backup`.

### 3. REST endpoints

Files:
- `crates/isengard-plugins/dashboard/src/backup.rs`: add 3 routes + handlers.
- DTOs: `RestoreRequestDto`, `RestoreOutcomeDto`, `RestoreRunDto`, `BackupRunManifestDto`.
- Routes:
  - `POST /backup/restore` -> `restore_now` handler.
  - `GET /backup/restore-runs` -> list handler.
  - `GET /backup/runs/:id/manifest` -> manifest handler.

Tests: 8 axum oneshot tests covering happy + sad paths.

Gate: `cargo test -p isengard-plugin-dashboard`.

### 4. UI

Files:
- `crates/isengard-plugins/dashboard/web/components/BackupRestoreModal.vue` (new).
- `crates/isengard-plugins/dashboard/web/components/BackupSettings.vue`: add a red "Restore from backup" button under the status panel; mount the modal; show the latest restore status.

Build verification: `cd crates/isengard-plugins/dashboard/web && bun install && bun run build`.

### 5. Docs + design status

Files:
- `design/pages/settings-backup.md`: bump status to Phase 11B shipped (full pipeline including restore).
- `docs/RELEASE_NOTES_PHASE_11B.md` (new): operator-facing walkthrough plus disaster-recovery example.

### 6. Gate sweep + PR

- `cargo build --workspace`
- `cargo test --workspace` (or nextest)
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo deny check`
- `bun run build` in `web/`

PR title: `feat: phase 11b (restore flow)`
PR body: "Closes #52" + summary + test plan.

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Atomic swap leaves DB in mixed state if the move fails mid-way | Always rename current to `.bak.<ts>` first. If the next move fails, restore the original from `.bak.<ts>`. Never delete silently. |
| Migrations fail on the restored file | Caller catches; we mark the run as failed and leave the `.bak.<ts>` in place so the operator can swap back manually. |
| Live `Inventory` pool keeps stale connections | Accept for v1: SQLite reopens transparently on next query under most filesystems. UI advises a controller restart. |
| WAL/SHM siblings get out of sync with the restored main file | Best-effort delete of `<db>-wal` and `<db>-shm` after the swap. The next pool connection rebuilds them. |
| Wrong-passphrase reveals attempts via DB rows | The error message is the typed enum's `Display`, which says "decryption failed: invalid passphrase or corrupted blob". No timing-sensitive content. |
| Race: scheduler fires a backup mid-restore | The backup runner takes an IMMEDIATE-tx lock; if the file is mid-rename it errors out and the run row records the failure. Operationally the operator should pause the scheduler before running a restore (UI guidance). |

## Acceptance

- [ ] Storage migration applies cleanly on a fresh DB.
- [ ] `cargo test -p isengard-storage` green with 4 new DAO tests.
- [ ] `cargo test -p isengard-plugin-backup` green with 6+ new restore tests.
- [ ] `cargo test -p isengard-plugin-dashboard` green with 8+ new endpoint tests.
- [ ] `cargo build --workspace` clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] `cargo deny check` clean.
- [ ] `bun run build` in `web/` clean.
- [ ] PR opened against `next` with `Closes #52`.
- [ ] `design/pages/settings-backup.md` updated; `docs/RELEASE_NOTES_PHASE_11B.md` written.
