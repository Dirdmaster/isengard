-- Phase 12b/c: lifecycle hook deliveries + external-action gate deliveries
-- share the existing 12a `webhook_deliveries` table. webhook_id becomes
-- nullable (lifecycle/gate rows have no parent webhook row); a `source`
-- discriminator tags each row; per-row url+secret carry the destination for
-- non-`webhook` rows.
--
-- See docs/superpowers/specs/2026-05-06-phase-12bc-lifecycle-hooks-and-gates-design.md
-- §Storage. Issues: #54 + #55.

-- SQLite cannot ALTER a NOT NULL away in place. Recreate the table by
-- copying through a shadow.

CREATE TABLE webhook_deliveries_v2 (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id      INTEGER REFERENCES webhooks(id) ON DELETE CASCADE,
    source          TEXT NOT NULL DEFAULT 'webhook'
                    CHECK (source IN ('webhook','lifecycle','gate')),
    url             TEXT,
    secret          TEXT,
    event_kind      TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    last_error      TEXT,
    next_retry_at   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

INSERT INTO webhook_deliveries_v2
  (id, webhook_id, source, url, secret, event_kind, payload_json,
   status, attempts, last_attempt_at, last_error, next_retry_at, created_at)
SELECT id, webhook_id, 'webhook', NULL, NULL, event_kind, payload_json,
       status, attempts, last_attempt_at, last_error, next_retry_at, created_at
FROM webhook_deliveries;

DROP TABLE webhook_deliveries;
ALTER TABLE webhook_deliveries_v2 RENAME TO webhook_deliveries;

CREATE INDEX idx_webhook_deliveries_status_retry
    ON webhook_deliveries(status, next_retry_at) WHERE status='pending';

CREATE INDEX idx_webhook_deliveries_source_created
    ON webhook_deliveries(source, created_at DESC);

CREATE INDEX idx_webhook_deliveries_webhook_created
    ON webhook_deliveries(webhook_id, created_at DESC);

-- Container-scope hook configuration. One row per (host, container_name).
-- Populated by the controller's HookLabelIngest from
-- `isengard.hooks.*` Docker labels. Reaped on container removal.
CREATE TABLE container_hooks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id         BLOB NOT NULL,
    container_id    TEXT NOT NULL,
    container_name  TEXT NOT NULL,
    pre_deploy_url  TEXT,
    post_deploy_url TEXT,
    on_failure_url  TEXT,
    secret          TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE(host_id, container_name)
);

CREATE INDEX idx_container_hooks_host_container
    ON container_hooks(host_id, container_id);
