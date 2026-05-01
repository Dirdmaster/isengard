CREATE TABLE fleets (
    name        TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT OR IGNORE INTO fleets (name) VALUES ('default');
