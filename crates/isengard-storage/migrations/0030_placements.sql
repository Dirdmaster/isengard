-- Phase 0.14: placement scheduler tables.
--
-- Numbered 0030 (the plan asked for 0031). Renumbered down because the
-- parent branch `feat/stack-file-parser` stops at 0029_containers.sql,
-- so 0030 is the next free slot and skipping it would leave a hole.
--
-- Adds:
-- 1. `agent_labels`: per-host label key/value pairs reported on every
--    heartbeat. Replaced wholesale when the agent's label set changes.
--    Empty for older agents (no labels in heartbeat).
-- 2. `placements`: scheduler-owned assignment of a service replica to a
--    host. One row per (service, replica_index) in steady state, plus
--    transient rows during drain/grace transitions. `state` covers the
--    lifecycle of one assignment from pending -> active -> draining /
--    failed.
-- 3. Backfill: every existing services row gets a single placements
--    row (replica_index=0, state='active') so the scheduler sees pre-
--    Phase-0.14 services as already-placed singletons and does not
--    churn them on first reconcile.
--
-- All new and no destructive changes; existing rows survive unchanged.

CREATE TABLE agent_labels (
    host_id   BLOB NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (host_id, key)
);

CREATE INDEX idx_agent_labels_host ON agent_labels(host_id);

CREATE TABLE placements (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    service_id      INTEGER NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    host_id         BLOB    NOT NULL REFERENCES hosts(id)    ON DELETE CASCADE,
    replica_index   INTEGER NOT NULL,
    state           TEXT    NOT NULL
                    CHECK(state IN ('pending','active','draining','failed')),
    assigned_at     TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_event      TEXT,
    UNIQUE(service_id, host_id, replica_index)
);

CREATE INDEX idx_placements_service ON placements(service_id);
CREATE INDEX idx_placements_host    ON placements(host_id);

-- Backfill: each existing service is treated as a placed singleton on
-- its current host. `last_seen_at` is stored as ISO-8601 text on the
-- services row so we can reuse it directly. Operator decision 2026-05-11
-- locked option A here.
INSERT INTO placements (service_id, host_id, replica_index, state, assigned_at)
    SELECT id, host_id, 0, 'active', last_seen_at FROM services;
