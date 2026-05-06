-- Phase 9b T1: extend host_actions for pending_approval rows.
-- See docs/superpowers/specs/2026-05-06-phase-9e-9f-approval-flow-design.md
-- section "Storage".
--
-- Existing schema (0006):
--   id            INTEGER PRIMARY KEY AUTOINCREMENT
--   host_id       BLOB NOT NULL REFERENCES hosts(id)
--   kind          TEXT NOT NULL                       -- no CHECK constraint
--   payload_json  TEXT NOT NULL DEFAULT '{}'
--   created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
--   delivered_at  TEXT
--   result        TEXT
--
-- The kind column has no CHECK constraint, so the new "update_pending_approval"
-- value is permitted as-is. We still need new columns to model the approval
-- lifecycle (state, expires_at, decided_at, decided_by, metadata_json) plus a
-- ULID action_id used as the externally-visible identifier.
--
-- Existing rows (force_update / decommission) get NULLs for these new columns
-- and continue to work via the existing `id INTEGER` path. Approval rows set
-- delivered_at = CURRENT_TIMESTAMP on insert so they don't accidentally bleed
-- into the agent's pending_actions stream (which filters delivered_at IS NULL).

ALTER TABLE host_actions ADD COLUMN action_id TEXT;
ALTER TABLE host_actions ADD COLUMN state TEXT;
ALTER TABLE host_actions ADD COLUMN expires_at TEXT;
ALTER TABLE host_actions ADD COLUMN decided_at TEXT;
ALTER TABLE host_actions ADD COLUMN decided_by TEXT;
ALTER TABLE host_actions ADD COLUMN metadata_json TEXT;
ALTER TABLE host_actions ADD COLUMN updated_at TEXT;

CREATE UNIQUE INDEX idx_host_actions_action_id ON host_actions(action_id)
    WHERE action_id IS NOT NULL;

CREATE INDEX idx_host_actions_state ON host_actions(state)
    WHERE state IS NOT NULL;

CREATE INDEX idx_host_actions_kind_state ON host_actions(kind, state)
    WHERE state IS NOT NULL;
