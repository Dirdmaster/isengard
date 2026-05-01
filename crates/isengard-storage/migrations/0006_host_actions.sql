CREATE TABLE host_actions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id       BLOB    NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL,
    payload_json  TEXT    NOT NULL DEFAULT '{}',
    created_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at  TEXT,
    result        TEXT
);
CREATE INDEX idx_host_actions_host_id_pending ON host_actions(host_id, delivered_at);
