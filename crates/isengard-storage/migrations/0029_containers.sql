-- Phase 0.18: containers as the leaf unit for `isd ps`.
--
-- 1. New `containers` table. Operator-visible id is a 16-char hex digest
--    of sha256(host_id || "|" || runtime_container_id) so the id is
--    globally unique per fleet and stable across reconnects. Backend's
--    native id (bollard / wisp) is kept alongside for inspect paths.
--
-- 2. Drop the bogus CHECK constraint on `services.state`. The constraint
--    only allowed four of the seven states the v0.5.3 enum defines, so
--    Pulling / Creating / Failed inserts failed silently and the row
--    fell back to Unknown. SQLite has no DROP CONSTRAINT, so we rebuild
--    the table.

CREATE TABLE containers (
    id                    TEXT    PRIMARY KEY,
    host_id               BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    service_id            INTEGER          REFERENCES services(id) ON DELETE SET NULL,
    runtime_container_id  TEXT    NOT NULL,
    image                 TEXT    NOT NULL,
    command               TEXT,
    state                 TEXT    NOT NULL,
    status_message        TEXT,
    names                 TEXT    NOT NULL,
    stack                 TEXT,
    service               TEXT,
    created_at            INTEGER,
    first_seen_at         INTEGER NOT NULL,
    last_seen_at          INTEGER NOT NULL,
    removed_at            INTEGER
);

CREATE INDEX containers_by_host  ON containers(host_id);
CREATE INDEX containers_by_stack ON containers(stack);
CREATE INDEX containers_alive    ON containers(host_id) WHERE removed_at IS NULL;

-- Rebuild services to drop the CHECK constraint introduced in 0005.
-- Migration 0012 added the deploy_strategy_override column, so the
-- rebuild must include it. We also preserve every existing row + the
-- two indexes from 0005.
CREATE TABLE services_new (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id                  BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id                 INTEGER          REFERENCES stacks(id) ON DELETE SET NULL,
    name                     TEXT    NOT NULL,
    image                    TEXT    NOT NULL,
    state                    TEXT    NOT NULL,
    last_seen_at             TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deploy_strategy_override TEXT    CHECK (deploy_strategy_override IS NULL
                                     OR deploy_strategy_override IN ('auto', 'blue-green', 'in-place')),
    UNIQUE(host_id, name)
);

INSERT INTO services_new (id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override)
    SELECT id, host_id, stack_id, name, image, state, last_seen_at, deploy_strategy_override
    FROM services;

DROP TABLE services;
ALTER TABLE services_new RENAME TO services;

CREATE INDEX idx_services_host_id  ON services(host_id);
CREATE INDEX idx_services_stack_id ON services(stack_id);
