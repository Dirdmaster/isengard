CREATE TABLE stacks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL,
    source          TEXT    NOT NULL CHECK(source IN ('compose', 'manual', 'inferred')),
    discovered_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(host_id, name)
);

CREATE INDEX idx_stacks_host_id ON stacks(host_id);
