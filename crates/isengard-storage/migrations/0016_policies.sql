-- Phase 9a: policies table for layered update-policy storage.
-- See docs/superpowers/specs/2026-05-06-phase-9a-9d-policy-foundation-design.md
-- section "Storage > Migration 0016".

CREATE TABLE policies (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_type  TEXT NOT NULL CHECK (scope_type IN
                  ('global', 'fleet', 'stack', 'service', 'container')),
    scope_key   TEXT NOT NULL,
    body_json   TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(scope_type, scope_key)
);

CREATE INDEX idx_policies_scope_type ON policies(scope_type);
