CREATE TABLE services (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id       BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    stack_id      INTEGER          REFERENCES stacks(id) ON DELETE SET NULL,
    name          TEXT    NOT NULL,
    image         TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK(state IN ('running', 'stopped', 'restarting', 'unknown')),
    last_seen_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(host_id, name)
);
CREATE INDEX idx_services_host_id  ON services(host_id);
CREATE INDEX idx_services_stack_id ON services(stack_id);
