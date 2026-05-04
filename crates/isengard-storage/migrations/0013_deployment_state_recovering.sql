-- Phase 10f: add `recovering` to the deployments.state CHECK constraint.
-- SQLite doesn't support ALTER ... CHECK, so we recreate the table.
-- The data preserves: copy → drop → rename.

CREATE TABLE deployments_new (
    id                       TEXT PRIMARY KEY,
    host_id                  BLOB NOT NULL,
    stack_id                 INTEGER NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    service_name             TEXT NOT NULL,
    strategy                 TEXT NOT NULL CHECK (strategy IN ('blue-green', 'in-place')),
    state                    TEXT NOT NULL CHECK (state IN (
        'pending', 'spinning_up', 'switching', 'draining',
        'destroying_blue', 'recovering', 'done', 'aborted', 'failed'
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
    created_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at               TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO deployments_new SELECT * FROM deployments;
DROP TABLE deployments;
ALTER TABLE deployments_new RENAME TO deployments;

CREATE INDEX idx_deployments_state_active
    ON deployments(state)
    WHERE state NOT IN ('done', 'failed', 'aborted');

CREATE INDEX idx_deployments_stack_created
    ON deployments(stack_id, created_at DESC);

CREATE INDEX idx_deployments_host_service_active
    ON deployments(host_id, service_name)
    WHERE state NOT IN ('done', 'failed', 'aborted');
