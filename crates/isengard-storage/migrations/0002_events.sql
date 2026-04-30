CREATE TABLE events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB,
    kind            TEXT NOT NULL,
    container_name  TEXT,
    image           TEXT,
    old_digest      TEXT,
    new_digest      TEXT,
    error           TEXT,
    summary         TEXT NOT NULL,
    metadata_json   TEXT,
    occurred_at     TEXT NOT NULL,
    received_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_events_host_id    ON events(host_id);
CREATE INDEX idx_events_kind       ON events(kind);
CREATE INDEX idx_events_occurred   ON events(occurred_at);
