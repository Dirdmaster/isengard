-- Phase 11a: backup_runs table for snapshot run history.
-- See docs/superpowers/specs/2026-05-06-phase-11a-backup-foundation-design.md
-- section "Run history".
--
-- Configuration itself rides the existing settings(key, value_json) table
-- under keys like backup.config.enabled, backup.config.destination, etc.
-- The encryption-key fingerprint also lives there. The actual passphrase
-- never touches the DB.

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
