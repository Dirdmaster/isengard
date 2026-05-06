-- Phase 10c (10i): multi-host deployment grouping + stack-level parallelism.
-- See docs/superpowers/specs/2026-05-06-phase-10h-10i-blue-green-history-and-rolling-design.md §Storage.

-- Stack-level parallelism setting.
-- Allowed values: NULL (defaults to 1), '1', '2'..'N', 'all'.
-- Stored as TEXT to preserve the 'all' sentinel without loss.
ALTER TABLE stacks ADD COLUMN deployment_parallelism TEXT;

-- Multi-host deployment grouping. One row per stack-wide rolling deploy.
CREATE TABLE deployment_groups (
    id              TEXT PRIMARY KEY,           -- ULID
    stack_id        INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name    TEXT NOT NULL,
    parallelism     TEXT NOT NULL,              -- snapshot at start
    state           TEXT NOT NULL CHECK (state IN ('pending', 'rolling', 'done', 'aborted', 'failed')),
    target_hosts    TEXT NOT NULL,              -- JSON array of host_id hex strings (lowercase, 32 chars each)
    started_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at     TEXT,
    error           TEXT
);

-- Hot path: surface in-flight groups for the orchestrator + dashboard.
CREATE INDEX idx_deployment_groups_state ON deployment_groups(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

-- Group listing per stack, newest first.
CREATE INDEX idx_deployment_groups_stack_started
    ON deployment_groups(stack_id, started_at DESC);

-- Per-deployment group reference. NULL for single-host (orchestrator-bypass) deploys.
ALTER TABLE deployments ADD COLUMN group_id TEXT REFERENCES deployment_groups(id);

-- Look up wave members efficiently.
CREATE INDEX idx_deployments_group ON deployments(group_id) WHERE group_id IS NOT NULL;
