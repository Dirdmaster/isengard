-- Phase 12a: outbound webhooks (#53).
-- See docs/superpowers/specs/2026-05-06-phase-12a-outbound-webhooks-design.md §Storage.

CREATE TABLE webhooks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    url          TEXT NOT NULL,
    secret       TEXT NOT NULL,
    event_kinds  TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id      INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event_kind      TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    last_error      TEXT,
    next_retry_at   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX idx_webhook_deliveries_status_retry
    ON webhook_deliveries(status, next_retry_at) WHERE status='pending';

CREATE INDEX idx_webhook_deliveries_webhook_created
    ON webhook_deliveries(webhook_id, created_at DESC);
