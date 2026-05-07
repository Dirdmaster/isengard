-- Phase 11b: restore_runs table for restore-from-backup history.
-- See docs/superpowers/specs/2026-05-06-phase-11b-restore-flow-design.md
-- section "Storage: restore_runs".
--
-- A restore points at a source backup object (and optionally the
-- backup_runs row that produced it), records the destination path of the
-- pre-restore DB (.bak.<ts>), and stores success/failure metadata. Kept in
-- a separate table from backup_runs because the operations are
-- semantically distinct (different metadata, different UI surface).

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
