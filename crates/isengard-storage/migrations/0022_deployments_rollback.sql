-- Phase 9F: rollback failure handler. Adds `previous_digest` +
-- `rollback_attempted_at` columns and extends the state CHECK to allow
-- `rolling_back`, `rolled_back`, `rollback_failed`.
--
-- SQLite doesn't support ALTER ... CHECK, so we recreate the table.
-- See docs/superpowers/specs/2026-05-06-phase-9f-rollback-handler-design.md.

CREATE TABLE deployments_new (
    id                       TEXT PRIMARY KEY,
    host_id                  BLOB NOT NULL,
    stack_id                 INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name             TEXT NOT NULL,
    strategy                 TEXT NOT NULL CHECK (strategy IN ('blue-green', 'in-place')),
    state                    TEXT NOT NULL CHECK (state IN (
        'pending', 'spinning_up', 'switching', 'draining',
        'destroying_blue', 'recovering',
        'rolling_back', 'rolled_back', 'rollback_failed',
        'done', 'aborted', 'failed'
    )),
    blue_container           TEXT,
    green_container          TEXT,
    blue_digest              TEXT NOT NULL,
    green_digest             TEXT NOT NULL,
    public_hostname          TEXT,
    health_path              TEXT,
    container_port           INTEGER,
    healthcheck_started_at   TEXT,
    healthcheck_passed_at    TEXT,
    switched_at              TEXT,
    drained_at               TEXT,
    finished_at              TEXT,
    error                    TEXT,
    metadata_json            TEXT,
    -- Phase 10c group reference, preserved.
    group_id                 TEXT REFERENCES deployment_groups(id),
    -- Phase 9F: previous-digest snapshot for the Rollback failure handler.
    -- NULL when on_failure != Rollback (deployment is not eligible for
    -- automatic rollback).
    previous_digest          TEXT,
    -- Timestamp the supervisor entered the rollback branch. NULL if no
    -- rollback was attempted. Set regardless of rollback success.
    rollback_attempted_at    TEXT,
    created_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Explicit column list (not SELECT *) so a future column reorder/add can't
-- silently mis-map data into the wrong slot during this recreate. New 9F
-- columns are NOT in the SELECT list because pre-9F rows don't have them;
-- their NULL default is correct.
INSERT INTO deployments_new (
    id, host_id, stack_id, service_name, strategy, state,
    blue_container, green_container, blue_digest, green_digest,
    public_hostname, health_path, container_port,
    healthcheck_started_at, healthcheck_passed_at, switched_at,
    drained_at, finished_at, error, metadata_json,
    group_id, created_at, updated_at
)
SELECT
    id, host_id, stack_id, service_name, strategy, state,
    blue_container, green_container, blue_digest, green_digest,
    public_hostname, health_path, container_port,
    healthcheck_started_at, healthcheck_passed_at, switched_at,
    drained_at, finished_at, error, metadata_json,
    group_id, created_at, updated_at
FROM deployments;
DROP TABLE deployments;
ALTER TABLE deployments_new RENAME TO deployments;

-- Re-create the indexes the prior migrations defined. The "active state"
-- predicate now also excludes rolled_back + rollback_failed (both terminal).
CREATE INDEX idx_deployments_state_active
    ON deployments(state)
    WHERE state NOT IN ('done', 'failed', 'aborted', 'rolled_back', 'rollback_failed');

CREATE INDEX idx_deployments_stack_created
    ON deployments(stack_id, created_at DESC);

CREATE INDEX idx_deployments_host_service_active
    ON deployments(host_id, service_name)
    WHERE state NOT IN ('done', 'failed', 'aborted', 'rolled_back', 'rollback_failed');

-- Phase 10c index: quick wave-member lookup. Recreated since the table was
-- swapped out under it.
CREATE INDEX idx_deployments_group ON deployments(group_id) WHERE group_id IS NOT NULL;
